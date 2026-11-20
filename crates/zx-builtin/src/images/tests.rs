use super::*;

/// A PNG of `width` by `height` pixels whose colours `pixel` gives.
fn png(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
    let picture = image::RgbImage::from_fn(width, height, |x, y| image::Rgb(pixel(x, y)));
    let mut bytes = Vec::new();
    DynamicImage::ImageRgb8(picture)
        .write_to(Cursor::new(&mut bytes), ImageFormat::Png)
        .unwrap();
    bytes
}

/// Colours that do not repeat, which no encoding compresses much.
fn noise(x: u32, y: u32) -> [u8; 3] {
    let n = x
        .wrapping_mul(7919)
        .wrapping_add(y.wrapping_mul(104_729))
        .wrapping_mul(2_654_435_761);
    let [a, b, c, _] = n.to_le_bytes();
    [a, b, c]
}

#[test]
fn an_image_within_the_limits_goes_as_it_is() {
    let bytes = png(3, 2, |_, _| [200, 10, 10]);
    let fitted = fit_within(&bytes, LIMITS).unwrap();
    assert_eq!(fitted.image.mime_type, "image/png");
    assert_eq!(fitted.image.data, STANDARD.encode(&bytes));
    assert_eq!((fitted.original, fitted.size), ((3, 2), (3, 2)));
    assert_eq!(fitted.scaling_note(), None);
}

#[test]
fn a_larger_image_is_scaled_to_fit_and_says_by_how_much() {
    let bytes = png(2400, 600, |_, _| [0, 0, 0]);
    let fitted = fit_within(&bytes, LIMITS).unwrap();
    assert_eq!(fitted.image.mime_type, "image/png");
    assert_eq!((fitted.original, fitted.size), ((2400, 600), (2000, 500)));
    assert_eq!(
        fitted.scaling_note().unwrap(),
        "[Image: original 2400x600, displayed at 2000x500. Multiply coordinates by 1.20 to map to original image.]"
    );
    let sent = STANDARD.decode(&fitted.image.data).unwrap();
    assert_eq!(image::load_from_memory(&sent).unwrap().dimensions(), (2000, 500));
}

#[test]
fn an_image_too_large_as_png_is_sent_as_jpeg_or_smaller_until_it_fits() {
    let bytes = png(64, 64, noise);
    let bounds = Bounds {
        side: 64,
        base64_bytes: 4000,
    };
    let fitted = fit_within(&bytes, bounds).unwrap();
    assert_eq!(fitted.image.mime_type, "image/jpeg");
    assert!(fitted.image.data.len() <= 4000, "{}", fitted.image.data.len());
    assert_eq!(fitted.original, (64, 64));
    let sent = STANDARD.decode(&fitted.image.data).unwrap();
    assert_eq!(image::load_from_memory(&sent).unwrap().dimensions(), fitted.size);
}

#[test]
fn an_image_that_no_encoding_brings_under_the_limit_is_refused() {
    let bytes = png(8, 8, noise);
    let bounds = Bounds {
        side: 8,
        base64_bytes: 10,
    };
    assert_eq!(
        fit_within(&bytes, bounds).unwrap_err(),
        "no encoding of the image is small enough"
    );
}

#[test]
fn files_of_other_kinds_are_refused() {
    for bytes in [&b"hello"[..], b"BM\x3a\0\0\0\0\0\0\0\x36\0\0\0\x28\0\0\0"] {
        assert_eq!(
            fit_within(bytes, LIMITS).unwrap_err(),
            "not a PNG, JPEG, GIF or WebP image"
        );
        assert!(!is_image(bytes));
    }
}

#[test]
fn a_broken_image_is_refused_before_it_is_sent() {
    let bytes = png(16, 16, noise);
    let error = fit_within(&bytes[..bytes.len() / 2], LIMITS).unwrap_err();
    assert!(error.starts_with("cannot decode the image: "), "{error}");
}

#[test]
fn the_formats_the_apis_take_are_images() {
    assert!(is_image(&png(1, 1, |_, _| [0, 0, 0])));
    assert!(is_image(b"\xff\xd8\xff\xe0\0\x10JFIF"));
    assert!(is_image(b"GIF89a\x01\0\x01\0"));
    assert!(is_image(b"RIFF\x1a\0\0\0WEBPVP8 "));
}

#[tokio::test]
async fn an_image_in_base64_is_decoded_then_fitted() {
    let bytes = png(2, 2, |_, _| [1, 2, 3]);
    let fitted = fit_base64(&STANDARD.encode(&bytes)).await.unwrap();
    assert_eq!(fitted.image.data, STANDARD.encode(&bytes));
    let error = fit_base64("not base64!").await.unwrap_err();
    assert!(error.starts_with("the image is not valid base64: "), "{error}");
}

#[test]
fn sizes_shrink_by_quarters_and_scale_into_a_square() {
    assert_eq!([shrunk(2000), shrunk(3), shrunk(1)], [1500, 2, 1]);
    assert_eq!(scaled_into((4000, 3000), 2000), (2000, 1500));
    assert_eq!(scaled_into((1, 5000), 2000), (1, 2000));
    assert_eq!(scaled_into((300, 200), 2000), (300, 200));
    assert_eq!(base64_len(4), 8);
}
