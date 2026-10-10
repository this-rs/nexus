//! The input of a turn as the CLI takes it.
//!
//! A text-only turn is one string: [`InteractiveClient::send_message`] writes
//! `"content": "<text>"`, byte for byte what the orchestrator wrote before the
//! contract. A turn with an image becomes a list of content blocks, written by
//! [`InteractiveClient::send_message_blocks`], in the **user's order**: the CLI
//! takes text and images interleaved (unlike the native harness, whose OpenAI
//! wire puts the joined text first).
//!
//! `send_message_blocks` leaves validation to its caller ("the CLI rejects an
//! image it cannot decode, so validate size and MIME type before calling"):
//! [`content_blocks`] refuses, before anything is written,
//!
//! - a media type outside [`IMAGE_MEDIA_TYPES`], the four types
//!   [`UserContentBlock::Image`] documents (and the ones `nexus-tools`' `Read`
//!   returns, the mirror of Claude Code's own);
//! - a payload that is not standard base64 (no `data:` prefix, no whitespace);
//! - a picture larger than [`MAX_IMAGE_BYTES`] once decoded, the cap of
//!   `nexus-tools`' `Read` (`MAX_IMAGE_BYTES`, 5 MiB), the per-image limit of the
//!   Anthropic API behind the CLI.
//!
//! Each refusal is `InvalidRequest`: the request is wrong, not the engine short of
//! a capability (a turn with an image is `Unsupported { images }` only when the
//! capability is absent).
//!
//! [`InteractiveClient::send_message`]: crate::InteractiveClient::send_message
//! [`InteractiveClient::send_message_blocks`]: crate::InteractiveClient::send_message_blocks

use crate::agent::{InputBlock, ProviderError, TurnInput};
use crate::transport::UserContentBlock;

/// Media types the CLI is given an image in.
pub const IMAGE_MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Largest decoded image of a turn, in bytes (5 MiB).
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// The blocks of `input`, in order, checked: what `send_message_blocks` sends.
///
/// An empty text block is left out (the API refuses an empty text block, and it
/// carries nothing); every other block keeps its place.
///
/// # Errors
///
/// [`ProviderError::InvalidRequest`] for an image the CLI would reject (see the
/// module documentation), or when nothing is left to send.
pub fn content_blocks(input: &TurnInput) -> Result<Vec<UserContentBlock>, ProviderError> {
    let mut blocks = Vec::with_capacity(input.blocks.len());
    for block in &input.blocks {
        match block {
            InputBlock::Text { text } if text.is_empty() => {},
            InputBlock::Text { text } => blocks.push(UserContentBlock::text(text.clone())),
            InputBlock::Image {
                media_type,
                data_base64,
            } => {
                check_image(media_type, data_base64)?;
                blocks.push(UserContentBlock::image_base64(
                    media_type.clone(),
                    data_base64.clone(),
                ));
            },
        }
    }
    if blocks.is_empty() {
        return Err(ProviderError::invalid("the turn has no content"));
    }
    Ok(blocks)
}

fn check_image(media_type: &str, data: &str) -> Result<(), ProviderError> {
    if !IMAGE_MEDIA_TYPES.contains(&media_type) {
        return Err(ProviderError::invalid(format!(
            "image media type `{media_type}` is not one of {}",
            IMAGE_MEDIA_TYPES.join(", ")
        )));
    }
    let decoded = decoded_len(data).ok_or_else(|| {
        ProviderError::invalid("an image payload must be standard base64, without a `data:` prefix")
    })?;
    if decoded > MAX_IMAGE_BYTES {
        return Err(ProviderError::invalid(format!(
            "an image is {decoded} bytes, more than the {MAX_IMAGE_BYTES} bytes the CLI accepts"
        )));
    }
    Ok(())
}

/// Decoded length of a standard, padded base64 payload; `None` when it is not one.
fn decoded_len(data: &str) -> Option<usize> {
    let bytes = data.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let padding = bytes.iter().rev().take_while(|&&b| b == b'=').count();
    if padding > 2 {
        return None;
    }
    let body = &bytes[..bytes.len() - padding];
    if !body
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/')
    {
        return None;
    }
    Some(bytes.len() / 4 * 3 - padding)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn image(media_type: &str, data: &str) -> InputBlock {
        InputBlock::Image {
            media_type: media_type.into(),
            data_base64: data.into(),
        }
    }

    fn text(text: &str) -> InputBlock {
        InputBlock::Text { text: text.into() }
    }

    #[test]
    fn blocks_keep_the_users_order() {
        let input = TurnInput {
            blocks: vec![
                image("image/png", PIXEL),
                text("between"),
                image("image/webp", "AAAA"),
                text(""),
                text("after"),
            ],
        };
        assert_eq!(
            content_blocks(&input).unwrap(),
            [
                UserContentBlock::image_base64("image/png", PIXEL),
                UserContentBlock::text("between"),
                UserContentBlock::image_base64("image/webp", "AAAA"),
                UserContentBlock::text("after"),
            ]
        );
    }

    #[test]
    fn every_documented_media_type_passes_and_no_other() {
        for media_type in IMAGE_MEDIA_TYPES {
            let input = TurnInput {
                blocks: vec![image(media_type, PIXEL)],
            };
            assert!(content_blocks(&input).is_ok(), "{media_type}");
        }
        for media_type in ["image/tiff", "image/svg+xml", "IMAGE/PNG", "text/plain", ""] {
            let input = TurnInput {
                blocks: vec![image(media_type, PIXEL)],
            };
            assert!(
                matches!(
                    content_blocks(&input),
                    Err(ProviderError::InvalidRequest { .. })
                ),
                "{media_type:?}"
            );
        }
    }

    #[test]
    fn a_payload_that_is_not_plain_base64_is_refused() {
        for data in [
            "",
            "AAA",
            "data:image/png;base64,AAAA",
            "AA AA",
            "AAAA\n",
            "A===",
            "AA-_",
        ] {
            let input = TurnInput {
                blocks: vec![image("image/png", data)],
            };
            assert!(
                matches!(
                    content_blocks(&input),
                    Err(ProviderError::InvalidRequest { .. })
                ),
                "{data:?}"
            );
        }
    }

    #[test]
    fn the_size_cap_is_on_the_decoded_bytes() {
        assert_eq!(decoded_len("AAAA"), Some(3));
        assert_eq!(decoded_len("AAA="), Some(2));
        assert_eq!(decoded_len("AA=="), Some(1));
        let at_cap = "A".repeat(MAX_IMAGE_BYTES / 3 * 4) + "AAA=";
        assert_eq!(decoded_len(&at_cap), Some(MAX_IMAGE_BYTES / 3 * 3 + 2));
        assert!(decoded_len(&at_cap).unwrap() <= MAX_IMAGE_BYTES);
        let input = TurnInput {
            blocks: vec![image("image/png", &at_cap)],
        };
        assert!(content_blocks(&input).is_ok());
        let over = "A".repeat((MAX_IMAGE_BYTES / 3 + 1) * 4);
        let input = TurnInput {
            blocks: vec![image("image/png", &over)],
        };
        assert!(matches!(
            content_blocks(&input),
            Err(ProviderError::InvalidRequest { .. })
        ));
    }

    #[test]
    fn nothing_to_send_is_refused() {
        let input = TurnInput {
            blocks: vec![text("")],
        };
        assert!(matches!(
            content_blocks(&input),
            Err(ProviderError::InvalidRequest { .. })
        ));
    }
}
