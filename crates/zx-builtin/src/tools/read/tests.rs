use std::path::Path;

use tokio_util::sync::CancellationToken;

use super::*;

fn ctx(dir: &Path) -> ToolContext {
    ToolContext::new(dir.to_path_buf(), CancellationToken::new())
}

#[tokio::test]
async fn pages_through_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let content: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    std::fs::write(dir.path().join("f.txt"), content).unwrap();
    let tool = ReadFileTool::default();

    let page = tool
        .execute(json!({"path": "@f.txt", "offset": 3, "limit": 2}), &ctx(dir.path()))
        .await
        .unwrap();
    assert_eq!(
        page.content,
        "line 3\nline 4\n[showing lines 3-4 of 10; use offset=5 to continue]"
    );

    let rest = tool
        .execute(json!({"path": "f.txt", "offset": 9}), &ctx(dir.path()))
        .await
        .unwrap();
    assert_eq!(rest.content, "line 9\nline 10\n");
}

#[tokio::test]
async fn empty_files_say_so() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("e.txt"), "").unwrap();
    let output = ReadFileTool::default()
        .execute(json!({"path": "e.txt"}), &ctx(dir.path()))
        .await
        .unwrap();
    assert_eq!(output.content, "(empty file)");
}

#[tokio::test]
async fn a_cancelled_read_does_not_return_file_contents() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "must not be returned").unwrap();
    let context = ctx(dir.path());
    context.cancel.cancel();

    let result = ReadFileTool::default()
        .execute(json!({"path": "f.txt"}), &context)
        .await;

    assert!(result.is_err(), "a cancelled read returned {result:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn refuses_special_files() {
    let result = ReadFileTool::default()
        .execute(json!({"path": "/dev/null"}), &ctx(Path::new("/")))
        .await;

    assert!(result.is_err(), "a special file was accepted: {result:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn refuses_named_pipes() {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fifo");
    crate::test_support::make_fifo(&path);
    let writer_path = path.clone();
    // This writer also lets the old implementation complete, so a failing regression
    // test never leaves a blocking read behind during runtime shutdown.
    let writer = tokio::task::spawn_blocking(move || {
        let mut file = std::fs::OpenOptions::new().write(true).open(writer_path).unwrap();
        file.write_all(b"pipe contents")
    });
    let result = ReadFileTool::default()
        .execute(json!({"path": "fifo"}), &ctx(dir.path()))
        .await;
    // Release the writer if the reader rejected the FIFO before opening it.
    let _unblock = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .unwrap();
    // Broken pipe is expected when rejection closes the read descriptor first.
    let _ = writer.await.unwrap();
    assert!(result.unwrap_err().contains("not a regular file"));
}

#[cfg(unix)]
#[tokio::test]
async fn refusing_a_named_pipe_does_not_open_it() {
    use std::os::unix::fs::OpenOptionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fifo");
    crate::test_support::make_fifo(&path);
    let writer_path = path.clone();
    // Opening a FIFO for writing waits until something opens it for reading: the writer
    // is released only if the reader opens the FIFO it refuses, as opening a device would
    // set off whatever the device does on open.
    let mut writer =
        tokio::task::spawn_blocking(move || std::fs::OpenOptions::new().write(true).open(writer_path).map(drop));
    let result = ReadFileTool::default()
        .execute(json!({"path": "fifo"}), &ctx(dir.path()))
        .await;
    // Showing that the writer stays blocked needs a bounded wait. A reader that does not
    // open the FIFO never releases it, so the wait cannot fail a correct implementation.
    let released = tokio::time::timeout(std::time::Duration::from_millis(300), &mut writer)
        .await
        .is_ok();
    if !released {
        let _unblock = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        writer.await.unwrap().unwrap();
    }
    assert!(result.unwrap_err().contains("not a regular file"));
    assert!(!released, "refusing the FIFO opened it");
}

#[tokio::test]
async fn long_files_end_with_a_hint_to_continue() {
    let dir = tempfile::tempdir().unwrap();
    let content: String = (1..=MAX_LINES + 5).map(|i| format!("{i}\n")).collect();
    std::fs::write(dir.path().join("long.txt"), content).unwrap();
    let output = ReadFileTool::default()
        .execute(json!({"path": "long.txt"}), &ctx(dir.path()))
        .await
        .unwrap();
    let expected = format!(
        "[showing lines 1-{MAX_LINES} of {}; use offset={} to continue]",
        MAX_LINES + 5,
        MAX_LINES + 1
    );
    assert!(
        output.content.ends_with(&expected),
        "{}",
        &output.content[output.content.len() - 80..]
    );
}

#[tokio::test]
async fn a_line_over_the_byte_limit_is_reported_as_cut() {
    let dir = tempfile::tempdir().unwrap();
    // One line, as in minified JSON. The byte limit falls inside a 3-byte character.
    let line = "界".repeat(MAX_BYTES / 3 + 10);
    std::fs::write(dir.path().join("one.json"), &line).unwrap();
    let output = ReadFileTool::default()
        .execute(json!({"path": "one.json"}), &ctx(dir.path()))
        .await
        .unwrap();
    let (shown, note) = output
        .content
        .rsplit_once('\n')
        .expect("a cut line must be followed by a note");
    assert!(
        line.starts_with(shown) && shown.len() <= MAX_BYTES,
        "{} bytes shown",
        shown.len()
    );
    assert_eq!(
        note,
        format!(
            "[line 1 of 1 was truncated to {} of its {} bytes; read_file cannot page within a line, \
                 so read the rest with another tool, such as bash]",
            shown.len(),
            line.len()
        )
    );
}

#[tokio::test]
async fn a_cut_line_is_not_counted_as_shown_when_more_lines_follow() {
    let dir = tempfile::tempdir().unwrap();
    let long = "x".repeat(MAX_BYTES + 100);
    std::fs::write(dir.path().join("two.txt"), format!("{long}\nnext\n")).unwrap();
    let output = ReadFileTool::default()
        .execute(json!({"path": "two.txt"}), &ctx(dir.path()))
        .await
        .unwrap();
    assert!(output.content.starts_with(&long[..MAX_BYTES]));
    assert_eq!(
        &output.content[MAX_BYTES..],
        format!(
            "\n[line 1 of 2 was truncated to {MAX_BYTES} of its {} bytes; read_file cannot page within a \
                 line, so read the rest with another tool, such as bash; use offset=2 to continue]",
            long.len()
        )
    );
}

#[tokio::test]
async fn a_line_that_fits_the_byte_limit_is_not_reported_as_cut() {
    let dir = tempfile::tempdir().unwrap();
    let line = "y".repeat(MAX_BYTES);
    std::fs::write(dir.path().join("fits.txt"), format!("{line}\nnext\n")).unwrap();
    let output = ReadFileTool::default()
        .execute(json!({"path": "fits.txt"}), &ctx(dir.path()))
        .await
        .unwrap();
    assert!(output.content.starts_with(&line));
    assert_eq!(
        &output.content[MAX_BYTES..],
        "\n[showing lines 1-1 of 2; use offset=2 to continue]"
    );
}

#[tokio::test]
async fn refuses_files_over_the_size_limit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big.txt"), "hello\n").unwrap();
    let tool = ReadFileTool {
        max_file_bytes: 4,
        ..ReadFileTool::default()
    };
    let error = tool
        .execute(json!({"path": "big.txt"}), &ctx(dir.path()))
        .await
        .unwrap_err();
    assert!(error.contains("over the 4 byte limit"), "{error}");
}

#[tokio::test]
async fn reads_exact_size_limits_and_large_page_limits_without_overflow() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("exact.txt"), "ab\nc").unwrap();
    let tool = ReadFileTool {
        max_file_bytes: 4,
        ..ReadFileTool::default()
    };
    let output = tool
        .execute(
            json!({"path": "exact.txt", "offset": 2, "limit": usize::MAX}),
            &ctx(dir.path()),
        )
        .await
        .unwrap();
    assert_eq!(output.content, "c");
}

#[tokio::test]
async fn rejects_unknown_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let error = ReadFileTool::default()
        .execute(json!({"path": "x", "lines": 3}), &ctx(dir.path()))
        .await
        .unwrap_err();
    assert!(error.contains("unknown field"), "{error}");
}

#[tokio::test]
async fn refuses_directories_binaries_and_bad_offsets() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("b.bin"), [0u8, 1, 2]).unwrap();
    std::fs::write(dir.path().join("t.txt"), "a\n").unwrap();
    let tool = ReadFileTool::default();
    let c = ctx(dir.path());
    assert!(
        tool.execute(json!({"path": "."}), &c)
            .await
            .unwrap_err()
            .contains("directory")
    );
    assert!(
        tool.execute(json!({"path": "b.bin"}), &c)
            .await
            .unwrap_err()
            .contains("binary")
    );
    assert!(
        tool.execute(json!({"path": "t.txt", "offset": 5}), &c)
            .await
            .unwrap_err()
            .contains("beyond")
    );
    assert!(tool.execute(json!({"path": "missing"}), &c).await.is_err());
}

/// A PNG of `width` by `height` black pixels.
fn black_png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::DynamicImage::new_rgb8(width, height)
        .write_to(std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .unwrap();
    bytes
}

#[tokio::test]
async fn an_image_is_returned_as_an_image() {
    use base64::Engine;

    let dir = tempfile::tempdir().unwrap();
    let bytes = black_png(4, 3);
    std::fs::write(dir.path().join("shot.png"), &bytes).unwrap();

    let read = ReadFileTool::default()
        .execute(json!({"path": "shot.png"}), &ctx(dir.path()))
        .await
        .unwrap();

    assert_eq!(read.content, "Read image file [image/png]");
    assert_eq!(read.images.len(), 1);
    assert_eq!(read.images[0].mime_type, "image/png");
    assert_eq!(
        read.images[0].data,
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    );
}

#[tokio::test]
async fn a_large_image_is_scaled_and_the_model_told_by_how_much() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.png"), black_png(3000, 100)).unwrap();

    let read = ReadFileTool::default()
        .execute(json!({"path": "wide.png"}), &ctx(dir.path()))
        .await
        .unwrap();

    assert_eq!(
        read.content,
        "Read image file [image/png]\n[Image: original 3000x100, displayed at 2000x67. Multiply coordinates by 1.50 to \
         map to original image.]"
    );
    assert_eq!(read.images.len(), 1);
}

#[tokio::test]
async fn a_broken_image_is_an_error_that_says_what_to_do() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = black_png(8, 8);
    std::fs::write(dir.path().join("cut.png"), &bytes[..bytes.len() / 2]).unwrap();

    let error = ReadFileTool::default()
        .execute(json!({"path": "cut.png"}), &ctx(dir.path()))
        .await
        .unwrap_err();

    assert!(
        error.contains("cut.png is an image that cannot be shown: cannot decode the image"),
        "{error}"
    );
    assert!(
        error.ends_with("use bash to convert it or to make it smaller"),
        "{error}"
    );
}
