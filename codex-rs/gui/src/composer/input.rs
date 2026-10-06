//! Assembly of the `Vec<UserInput>` sent for one composer submission, and
//! inlining of local images for servers on another machine.

use std::path::PathBuf;

use codex_app_server_protocol::UserInput;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ImageReference;
use codex_protocol::models::snapshot_local_user_input;
use codex_protocol::user_input::UserInput as CoreUserInput;

use super::text::dollar_mentions;

/// Total size of inlined images per message: the TUI's remote budget, which
/// leaves headroom below the WebSocket server's 64 MiB message limit.
pub(crate) const MAX_INLINE_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// A skill the composer can reference with `$name`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillRef {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) path: PathBuf,
}

/// Builds the turn input in the same order as the TUI: local images first,
/// then the text (when not blank), then one `Skill` item per distinct skill
/// mentioned as `$name` in the text.
pub(crate) fn build_user_input(
    text: &str,
    images: &[PathBuf],
    skills: &[SkillRef],
) -> Vec<UserInput> {
    let mut items: Vec<UserInput> = images
        .iter()
        .map(|path| UserInput::LocalImage {
            detail: None,
            path: path.clone(),
        })
        .collect();
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        items.push(UserInput::Text {
            text: trimmed.to_string(),
            text_elements: Vec::new(),
        });
    }
    let mut seen: Vec<&PathBuf> = Vec::new();
    for name in dollar_mentions(trimmed) {
        if let Some(skill) = skills.iter().find(|skill| skill.name == name)
            && !seen.contains(&&skill.path)
        {
            seen.push(&skill.path);
            items.push(UserInput::Skill {
                name: skill.name.clone(),
                path: skill.path.clone(),
            });
        }
    }
    items
}

/// Replaces every `LocalImage` in `input` with an inline data-URL `Image`
/// (read, validated and resized like the TUI's remote mode), keeping the
/// order. The encoded images may use at most `budget` bytes together.
///
/// Reads and decodes files: call it off the UI thread.
pub(crate) fn inline_local_images(
    input: Vec<UserInput>,
    budget: usize,
) -> Result<Vec<UserInput>, String> {
    let mut remaining = budget;
    input
        .into_iter()
        .map(|item| {
            let UserInput::LocalImage { path, .. } = item else {
                return Ok(item);
            };
            let failed =
                |error: std::io::Error| format!("Could not attach {}: {error}", path.display());
            let too_large = || {
                std::io::Error::other(format!(
                    "the images exceed the {} MiB limit for remote servers",
                    budget / (1024 * 1024)
                ))
            };
            // Base64 grows the file by a third; refuse before reading it.
            let size = std::fs::metadata(&path).map_err(failed)?.len();
            if size > (remaining / 4 * 3) as u64 {
                return Err(failed(too_large()));
            }
            let mut core = CoreUserInput::LocalImage {
                path: path.clone(),
                // Keep the source pixels; the server resizes per model.
                detail: Some(ImageDetail::Original),
            };
            snapshot_local_user_input(&mut core).map_err(failed)?;
            if let CoreUserInput::Image {
                image: ImageReference::Inline { image_url },
                ..
            } = &core
            {
                remaining = remaining
                    .checked_sub(image_url.len())
                    .ok_or_else(|| failed(too_large()))?;
            }
            let mut inlined = UserInput::from(core);
            if let UserInput::Image { detail, .. } = &mut inlined {
                *detail = None;
            }
            Ok(inlined)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn skill(name: &str, path: &str) -> SkillRef {
        SkillRef {
            name: name.to_string(),
            description: String::new(),
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn text_only_is_trimmed() {
        assert_eq!(
            build_user_input("  hello\n", &[], &[]),
            vec![UserInput::Text {
                text: "hello".to_string(),
                text_elements: Vec::new(),
            }]
        );
        assert_eq!(build_user_input(" \n ", &[], &[]), Vec::new());
    }

    #[test]
    fn images_come_before_text() {
        let input = build_user_input("what is this", &[PathBuf::from("/tmp/a.png")], &[]);
        assert_eq!(
            input,
            vec![
                UserInput::LocalImage {
                    detail: None,
                    path: PathBuf::from("/tmp/a.png"),
                },
                UserInput::Text {
                    text: "what is this".to_string(),
                    text_elements: Vec::new(),
                },
            ]
        );
        // Images alone are a valid message.
        assert_eq!(
            build_user_input("", &[PathBuf::from("/b.png")], &[]).len(),
            1
        );
    }

    fn write_png(dir: &std::path::Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        image::RgbaImage::from_pixel(4, 3, image::Rgba([10, 20, 30, 255]))
            .save_with_format(&path, image::ImageFormat::Png)
            .expect("png written");
        path
    }

    #[test]
    fn local_images_are_inlined_as_data_urls_in_order() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let png = write_png(dir.path(), "shot.png");
        let input = build_user_input("what is this", &[png], &[]);
        let inlined =
            inline_local_images(input, MAX_INLINE_IMAGE_BYTES).map_err(anyhow::Error::msg)?;
        assert_eq!(inlined.len(), 2);
        match &inlined[0] {
            UserInput::Image {
                image: codex_app_server_protocol::ImageReference::Inline { url },
                detail,
            } => {
                assert!(url.starts_with("data:image/png;base64,"));
                assert_eq!(*detail, None);
            }
            other => panic!("expected an inline image, got {other:?}"),
        }
        assert_eq!(
            inlined[1],
            UserInput::Text {
                text: "what is this".to_string(),
                text_elements: Vec::new(),
            }
        );
        Ok(())
    }

    #[test]
    fn inlining_enforces_the_size_budget_and_reports_missing_files() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let png = write_png(dir.path(), "shot.png");
        let error = inline_local_images(build_user_input("", &[png], &[]), /*budget*/ 16)
            .expect_err("over budget");
        assert!(error.contains("shot.png"), "{error}");
        assert!(error.contains("limit for remote servers"), "{error}");

        let missing = dir.path().join("gone.png");
        let error = inline_local_images(
            build_user_input("", &[missing], &[]),
            MAX_INLINE_IMAGE_BYTES,
        )
        .expect_err("missing file");
        assert!(error.contains("gone.png"), "{error}");
        Ok(())
    }

    #[test]
    fn known_skills_become_skill_items_once() {
        let skills = vec![
            skill("lint", "/skills/lint/SKILL.md"),
            skill("deploy", "/skills/deploy/SKILL.md"),
        ];
        let input = build_user_input("run $lint then $lint again, not $unknown", &[], &skills);
        assert_eq!(
            input,
            vec![
                UserInput::Text {
                    text: "run $lint then $lint again, not $unknown".to_string(),
                    text_elements: Vec::new(),
                },
                UserInput::Skill {
                    name: "lint".to_string(),
                    path: PathBuf::from("/skills/lint/SKILL.md"),
                },
            ]
        );
    }
}
