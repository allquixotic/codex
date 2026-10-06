//! Small, version-pinned Slint bridge. Keep internal APIs confined here.
//! Slint exposes link clicks, but not glyph hit-testing or full-frame invalidation.
use std::ops::ControlFlow;

use i_slint_core::item_tree::ItemRc;
use i_slint_core::item_tree::ParentItemTraversalMode;
use i_slint_core::items::StyledTextItem;
use i_slint_core::lengths::LogicalPoint;
use i_slint_core::lengths::LogicalRect;
use i_slint_core::lengths::LogicalSize;
use i_slint_core::lengths::ScaleFactor;
use i_slint_core::window::WindowInner;

/// Repaint lost pixels even when the presentation surface claims it retained them.
/// Called before a Windows redraw, without requesting another frame.
pub(crate) fn invalidate_frame(window: &slint::Window) {
    let inner = WindowInner::from_pub(window);
    let size = window.size().to_logical(window.scale_factor());
    let rect = LogicalRect::new(
        LogicalPoint::default(),
        LogicalSize::new(size.width, size.height),
    );
    inner
        .window_adapter()
        .renderer()
        .mark_dirty_region(rect.into());
}

/// Test the shaped glyphs, using the same layout as StyledText's left-click handler.
/// Only materialized, visible rows are visited; no history or layout cache is retained.
pub(crate) fn link_at(window: &slint::Window, x: f32, y: f32) -> Option<String> {
    // A point query must not subscribe the hover callback to the whole scene
    // (including its own tooltip). Such a dependency restarts the hover timer
    // when the tooltip appears. Discard query dependencies on return.
    let tracker = std::pin::pin!(i_slint_core::properties::PropertyTracker::<false>::default());
    tracker
        .as_ref()
        .evaluate_as_dependency_root(|| link_at_inner(window, x, y))
}

fn link_at_inner(window: &slint::Window, x: f32, y: f32) -> Option<String> {
    let inner = WindowInner::from_pub(window);
    let root = ItemRc::new_root(inner.try_component()?);
    let point = LogicalPoint::new(x, y);
    let scale = ScaleFactor::new(window.scale_factor());
    root.visit_descendants(|item| {
        let Some(text) = item.downcast::<StyledTextItem>() else {
            return ControlFlow::Continue(());
        };
        if !contains_visible_point(item, point) {
            return ControlFlow::Continue(());
        }
        let geo = item.geometry();
        let origin = item.map_to_window(geo.origin);
        let local = point - origin.to_vector();
        let text = text.as_pin_ref();
        match i_slint_core::textlayout::sharedparley::link_under_cursor(
            scale,
            text,
            item,
            LogicalSize::from_lengths(text.width(), text.height()),
            local * scale,
            window,
            None,
        ) {
            Some(link) => ControlFlow::Break(link),
            None => ControlFlow::Continue(()),
        }
    })
}

fn contains_visible_point(item: &ItemRc, point: LogicalPoint) -> bool {
    if !item.is_visible() || !window_rect(item).contains(point) {
        return false;
    }
    let mut parent = item.parent_item(ParentItemTraversalMode::StopAtPopups);
    while let Some(ancestor) = parent {
        if ancestor.borrow().as_ref().clips_children() && !window_rect(&ancestor).contains(point) {
            return false;
        }
        parent = ancestor.parent_item(ParentItemTraversalMode::StopAtPopups);
    }
    true
}

fn window_rect(item: &ItemRc) -> LogicalRect {
    let geo = item.geometry();
    LogicalRect::new(item.map_to_window(geo.origin), geo.size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::ComponentHandle;
    use slint::Rgb8Pixel;
    use slint::platform::Platform;
    use slint::platform::WindowAdapter;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use std::rc::Rc;

    slint::slint! {
        export component LinkScene inherits Window {
            width: 320px;
            height: 200px;
            background: #f2f2f2;
            Rectangle {
                x: 10px; y: 10px; width: 150px; height: 80px;
                background: #cceeff;
                clip: true;
                StyledText {
                    x: 5px; y: 5px; width: 100px; height: 150px;
                    default-font-size: 16px;
                    text: @markdown("[wrap wrap wrap wrap wrap wrap wrap wrap](https://example.com/a?b=1) plain [file](file:///tmp/a%20b.txt#L7)");
                }
            }
        }
    }

    struct Backend(Rc<MinimalSoftwareWindow>);
    impl Platform for Backend {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn v1_v2_v3_scroll_surface_loss_and_wrapped_link_hits() -> anyhow::Result<()> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        slint::platform::set_platform(Box::new(Backend(window.clone())))?;
        let scene = LinkScene::new()?;
        scene.window().set_size(slint::PhysicalSize::new(320, 200));
        scene.show()?;
        slint::platform::update_timers_and_animations();
        let mut pixels = vec![Rgb8Pixel::default(); 320 * 200];
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 320);
        });
        let expected = pixels.clone();
        assert!(expected.iter().any(|p| p.r > 0));
        // Model an RDP surface losing pixels while the renderer claims reuse.
        pixels.fill(Rgb8Pixel::default());
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 320);
        });
        assert_ne!(
            pixels, expected,
            "baseline must reproduce retained-buffer loss"
        );
        invalidate_frame(scene.window());
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 320);
        });
        assert_eq!(pixels, expected, "full invalidation must repair all pixels");
        assert!(!window.draw_if_needed(|_| panic!("idle must not poll repaint")));

        let mut link_rows = std::collections::HashSet::new();
        let mut file_hit = false;
        for y in (15..90).step_by(2) {
            for x in (15..115).step_by(2) {
                if let Some(url) = link_at(scene.window(), x as f32, y as f32) {
                    match url.as_str() {
                        "https://example.com/a?b=1" => {
                            link_rows.insert(y / 20);
                        }
                        "file:///tmp/a%20b.txt#L7" => file_hit = true,
                        _ => panic!("unexpected glyph destination: {url}"),
                    }
                }
            }
        }
        assert!(link_rows.len() >= 2, "hit wrapped link on multiple lines");
        assert!(
            file_hit,
            "hit the adjacent file link without confusing destinations"
        );
        assert_eq!(
            link_at(scene.window(), 20.0, 95.0),
            None,
            "clipped glyphs must not hit"
        );
        assert_eq!(link_at(scene.window(), 250.0, 20.0), None);
        scene.hide()?;
        drop(scene);

        let app = crate::ui::MainWindow::new()?;
        let state = app.global::<crate::ui::AppState>();
        state.set_has_active_tab(true);
        state.set_active_kind(crate::ui::TabKindCode::Thread);
        state.set_server_status("ready".into());
        state.set_sidebar_visible(false);
        state.set_info_visible(false);
        let transcript = app.global::<crate::ui::TranscriptState>();
        transcript.on_link_destination(|url| url);
        let rows: Vec<_> = (0..40)
            .map(|i| crate::ui::BlockData {
                id: format!("row{i}").into(),
                kind: 0,
                message: true,
                rich: slint::StyledText::from_plain_text(
                    &"variable height text ".repeat(8 + i % 5),
                ),
                ..Default::default()
            })
            .collect();
        let model = Rc::new(slint::VecModel::from(rows));
        transcript.set_blocks(model.clone().into());
        app.window().set_size(slint::PhysicalSize::new(800, 600));
        app.show()?;
        let mut pixels = vec![Rgb8Pixel::default(); 800 * 600];
        for _ in 0..8 {
            slint::platform::update_timers_and_animations();
            window.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, 800);
            });
        }
        for i in 40..45 {
            model.push(crate::ui::BlockData {
                id: format!("stream{i}").into(),
                kind: 0,
                message: true,
                rich: slint::StyledText::from_plain_text(&"streamed text ".repeat(10)),
                ..Default::default()
            });
            for _ in 0..8 {
                slint::platform::update_timers_and_animations();
                window.draw_if_needed(|renderer| {
                    renderer.render(&mut pixels, 800);
                });
            }
            assert!(
                transcript.get_follow_tail(),
                "virtual-height updates preserve live tail following"
            );
        }
        let before = transcript.get_scroll_y();
        assert!(before < -100.0, "start at latest message: {before}");
        assert!(
            transcript.get_follow_tail(),
            "initial layout preserves tail following"
        );
        let rect = transcript.get_viewport();
        app.window()
            .dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                position: slint::LogicalPosition::new(rect.x + 40.0, rect.y + 40.0),
                delta_x: 0.0,
                delta_y: 10.0,
            });
        for _ in 0..8 {
            slint::platform::update_timers_and_animations();
            window.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, 800);
            });
        }
        assert!(
            !transcript.get_follow_tail(),
            "small idle wheel must detach"
        );
        assert!(
            transcript.get_scroll_y() > before + 1.0,
            "upward scroll must persist"
        );
        let weak = app.as_weak();
        transcript.on_link_at(move |x, y| {
            let result = weak
                .upgrade()
                .and_then(|app| link_at(app.window(), x, y))
                .unwrap_or_default();
            result.into()
        });
        transcript.set_blocks(
            Rc::new(slint::VecModel::from(vec![crate::ui::BlockData {
                id: "link".into(),
                kind: 0,
                message: true,
                rich: slint::StyledText::from_markdown("[website](https://example.com/a?b=1)")?,
                ..Default::default()
            }]))
            .into(),
        );
        transcript.set_scroll_top_request(1);
        for _ in 0..8 {
            slint::platform::update_timers_and_animations();
            window.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, 800);
            });
        }
        let mut hit = None;
        'scan: for y in (rect.y as i32..(rect.y + rect.height) as i32).step_by(4) {
            for x in (rect.x as i32..(rect.x + rect.width) as i32).step_by(4) {
                if link_at(app.window(), x as f32, y as f32).is_some() {
                    hit = Some(slint::LogicalPosition::new(x as f32, y as f32));
                    break 'scan;
                }
            }
        }
        let hit = hit.expect("visible hyperlink glyph");
        let hit = slint::LogicalPosition::new(hit.x + 12.0, hit.y + 8.0);
        assert!(link_at(app.window(), hit.x, hit.y).is_some());
        app.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: hit });
        slint::platform::update_timers_and_animations();
        std::thread::sleep(std::time::Duration::from_millis(350));
        slint::platform::update_timers_and_animations();
        app.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(hit.x + 1.0, hit.y),
            });
        slint::platform::update_timers_and_animations();
        std::thread::sleep(std::time::Duration::from_millis(350));
        slint::platform::update_timers_and_animations();
        assert!(
            !transcript.get_link_hover_visible(),
            "movement resets the stationary-hover delay"
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
        slint::platform::update_timers_and_animations();
        assert!(
            transcript.get_link_hover_visible(),
            "stationary hover shows destination"
        );
        assert_eq!(
            transcript.get_link_hover_text(),
            "https://example.com/a?b=1"
        );
        app.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(hit.x + 2.0, hit.y),
            });
        assert!(
            !transcript.get_link_hover_visible(),
            "even movement within link dismisses tooltip"
        );
        app.hide()?;
        Ok(())
    }
}
