//! The text of an error with its causes.

/// `error` followed by its causes. A library's own message often omits the cause, such as
/// "Connection refused", which is what an unattended run needs for diagnosis. A cause whose
/// text is already included is skipped.
pub fn with_causes(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use super::*;

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);

    impl fmt::Display for Layer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1
                .as_deref()
                .map(|cause| cause as &(dyn std::error::Error + 'static))
        }
    }

    #[test]
    fn the_causes_follow_the_error_and_a_repeated_one_is_left_out() {
        let error = Layer(
            "request failed: connection refused",
            Some(Box::new(Layer(
                "connection refused",
                Some(Box::new(Layer("os error 61", None))),
            ))),
        );
        assert_eq!(with_causes(&error), "request failed: connection refused: os error 61");
    }
}
