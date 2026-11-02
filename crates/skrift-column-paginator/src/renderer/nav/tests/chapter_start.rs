use super::*;
use crate::PaintDirection;

#[test]
fn backward_short_chapters_preserve_authored_start_geometry() {
    let viewport = Size::new(600.0, 500.0);
    let next = "<html><body>Next chapter</body></html>";
    for (style, prefix) in [
        ("margin:24px", ""),
        ("margin:0;padding:32px", ""),
        (
            "margin:0",
            "<div style='height:80px;background:green'></div>",
        ),
    ] {
        let source = format!(
            "<html><body style='{style}'>{prefix}<p style='margin:16px 0;line-height:28px'>First</p><p style='margin:16px 0;line-height:28px'>Second</p><p style='margin:16px 0;line-height:28px'>Third</p></body></html>"
        );
        for columns in [1, 2] {
            for command in 0..3 {
                let (mut core, mut shaper) = spine_core(
                    vec![("short.html", &source), ("next.html", next)],
                    viewport,
                    viewport.width / f64::from(columns) - 40.0,
                    columns,
                );
                assert_eq!(core.page.layout.col_count as u8, columns);
                let original = visible_positions(&core);
                assert!(
                    original[0].2 > 0.0,
                    "the first line must retain authored leading space"
                );
                core.next_document(&mut shaper);
                core.prepare_frame(&mut shaper, viewport);
                match command {
                    0 => core.prev_page(&mut shaper),
                    1 => core.prev_document(&mut shaper),
                    _ => core.prev_line(&mut shaper),
                }
                core.prepare_frame(&mut shaper, viewport);
                assert_eq!(core.nav.location().document(), 0);
                assert!(matches!(
                    core.viewport.composition().direction(),
                    PaintDirection::Forward
                ));
                assert_eq!(core.viewport.composition().start_offset_y(), 0.0);
                assert_eq!(
                    visible_positions(&core),
                    original,
                    "returning must reproduce forward geometry ({style}, columns={columns}, command={command})"
                );
                core.next_page(&mut shaper);
                core.prepare_frame(&mut shaper, viewport);
                assert_eq!(
                    core.nav.location().document(),
                    1,
                    "the next turn must leave the short chapter immediately"
                );
            }
        }
    }
}

#[test]
fn uncached_backward_first_page_preserves_leading_spacing() {
    let viewport = Size::new(200.0, 80.0);
    let source = "<html><body style='margin:10px'><div style='line-height:20px'>A</div><div style='line-height:20px'>B</div><div style='line-height:20px'>C</div><div style='line-height:20px'>D</div><div style='line-height:20px'>E</div><div style='line-height:20px'>F</div></body></html>";
    let (mut core, mut shaper) = pagination_core(source, viewport, 1);
    let original = visible_positions(&core);
    assert!(original[0].2 > 0.0);
    core.next_page(&mut shaper);
    core.prepare_frame(&mut shaper, viewport);
    assert_ne!(visible_positions(&core), original);
    core.prev_page(&mut shaper);
    core.prepare_frame(&mut shaper, viewport);
    assert_eq!(visible_positions(&core), original);
    assert_eq!(core.viewport.composition().start_offset_y(), 0.0);
    assert!(matches!(
        core.viewport.composition().direction(),
        PaintDirection::Forward
    ));
}

#[test]
fn backward_long_chapters_preserve_the_last_page_with_leading_spacing() {
    let viewport = Size::new(300.0, 200.0);
    let paragraphs = (0..80)
        .map(|index| format!("<p style='margin:0;line-height:28px'>Paragraph {index}</p>"))
        .collect::<String>();
    let source = format!("<html><body style='margin:24px;padding:16px'>{paragraphs}</body></html>");
    for command in 0..3 {
        let (mut core, mut shaper) = spine_core(
            vec![
                ("long.html", &source),
                ("next.html", "<html><body>Next chapter</body></html>"),
            ],
            viewport,
            viewport.width,
            1,
        );
        core.next_document(&mut shaper);
        core.prepare_frame(&mut shaper, viewport);
        match command {
            0 => core.prev_page(&mut shaper),
            1 => core.prev_document(&mut shaper),
            _ => core.prev_line(&mut shaper),
        }
        core.prepare_frame(&mut shaper, viewport);
        assert_eq!(core.nav.location().document(), 0);
        assert!(core.page.frame.page().start_line().unwrap() > 0);
        assert_eq!(
            core.page.frame.page().end_line(),
            Some(core.page.render_view().text().line_count() - 1)
        );
        assert!(matches!(
            core.viewport.composition().direction(),
            PaintDirection::Backward
        ));
    }
}
