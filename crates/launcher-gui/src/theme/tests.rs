use super::*;

#[test]
fn desktop_sans_draws_regular_and_bold_labels() {
    use iced::advanced::graphics::text::font_system;

    init_fonts();
    let mut system = font_system().write().unwrap();
    let system = system.raw();
    let family = system
        .db()
        .family_name(&fontdb::Family::SansSerif)
        .to_owned();
    println!("Selected interface font: {family}");
    assert_sans_draws(system, &family);
}

#[test]
fn bundled_sans_draws_without_system_fonts() {
    use iced::advanced::graphics::text::{cosmic_text, FIRA_SANS_REGULAR};

    let mut db = fontdb::Database::new();
    db.load_font_data(FIRA_SANS_REGULAR.to_vec());
    let mut system = cosmic_text::FontSystem::new_with_locale_and_db("en-US".to_owned(), db);
    assert_eq!(
        select_sans(
            &mut system,
            Some("Missing Desktop Font 10"),
            Some("Missing Sans")
        ),
        "Fira Sans"
    );
    assert_sans_draws(&mut system, "Fira Sans");
}

#[test]
fn desktop_font_description_preserves_multiword_and_numbered_families() {
    assert_eq!(font_family("Inter Variable 10"), "Inter Variable");
    assert_eq!(font_family("Noto Sans 10.5"), "Noto Sans");
    assert_eq!(font_family("DIN 1451 12px"), "DIN 1451");
    assert_eq!(font_family("Fira Sans"), "Fira Sans");
}

fn assert_sans_draws(system: &mut FontSystem, family: &str) {
    use iced::advanced::graphics::text::cosmic_text::{
        Attrs, Buffer, Family, Metrics, Shaping, Weight,
    };

    for weight in [Weight::NORMAL, Weight::BOLD] {
        let mut buffer = Buffer::new(system, Metrics::new(16.0, 20.0));
        buffer.set_size(system, Some(1000.0), Some(100.0));
        buffer.set_text(
            system,
            "Welcome to Lightforge! Get started",
            &Attrs::new().family(Family::SansSerif).weight(weight),
            Shaping::Basic,
            None,
        );
        buffer.shape_until_scroll(system, false);
        let glyphs: Vec<_> = buffer.layout_runs().flat_map(|run| run.glyphs).collect();
        assert!(!glyphs.is_empty());
        for glyph in glyphs {
            assert_ne!(
                glyph.glyph_id, 0,
                "missing label glyph at weight {weight:?}"
            );
            assert!(system
                .db()
                .face(glyph.font_id)
                .unwrap()
                .families
                .iter()
                .any(|(name, _)| name == family));
        }
    }
}

#[test]
fn text_on_a_state_colour_stays_readable() {
    // Install, Play and Update are all light enough to need dark text;
    // green OK is the one that needs white. Getting this backwards makes
    // the one button that matters unreadable.
    assert!(on(INSTALL).r < 0.5, "light backgrounds need dark text");
    assert!(on(PLAY).r < 0.5, "the wordmark's gold needs dark text");
    assert!(on(UPDATE).r < 0.5, "violet is light enough for dark text");
    assert_eq!(on(OK), Color::WHITE);
}

/// The point of the whole exercise: marks of wildly different shape come
/// out covering the same amount of page.
#[test]
fn marks_of_any_shape_are_given_the_same_area() {
    // Mists' trimmed ink against Forever's: 2.0:1 and 1.2:1, the widest
    // and the narrowest the box does not cut back.
    let (mw, mh) = logo_size(704.0 / 353.0, 1.0);
    let (fw, fh) = logo_size(538.0 / 440.0, 1.0);
    let (wide, square) = (mw * mh, fw * fh);
    assert!((wide - square).abs() / square < 0.01, "{wide} vs {square}");
    for (w, h) in [(mw, mh), (fw, fh)] {
        assert!(w <= LOGO_BOX_WIDTH && h <= LOGO_BOX_HEIGHT, "{w}x{h}");
    }
}

/// Equal area is the rule; the box is the exception to it, and StarCraft
/// is the one mark that meets the exception; see [`LOGO_BOX_WIDTH`].
#[test]
fn the_one_mark_too_wide_for_the_page_is_cut_back() {
    let (sw, sh) = logo_size(704.0 / 127.0, 1.0);
    assert_eq!(sw, LOGO_BOX_WIDTH);
    assert!(sw * sh < LOGO_AREA * 0.8, "{}", sw * sh);
    // The next widest is not touched: cutting a second mark back would
    // make this a rule about wide marks, not about this one.
    let (w2, h2) = logo_size(704.0 / 202.0, 1.0);
    assert!(w2 < LOGO_BOX_WIDTH);
    assert!((w2 * h2 - LOGO_AREA).abs() / LOGO_AREA < 0.01);
}

/// Shape nobody has published: made smaller, never pushed off the page.
#[test]
fn an_extreme_shape_is_clamped_into_the_box() {
    let (w, h) = logo_size(20.0, 1.0);
    assert!(w <= LOGO_BOX_WIDTH && h <= LOGO_BOX_HEIGHT, "{w}x{h}");
    assert!(w * h < LOGO_AREA, "a clamped mark is smaller, not larger");
}

/// An aspect that is not a number at all is a square, not a panic.
#[test]
fn a_nonsense_aspect_is_treated_as_square() {
    for nonsense in [f32::NAN, 0.0, -3.0, f32::INFINITY] {
        let (w, h) = logo_size(nonsense, 1.0);
        assert!((w - h).abs() < 0.5, "{nonsense}: {w}x{h}");
    }
}

/// Retail's mark sits in a halo a third as wide again as itself. Measured
/// by its outer bounds it comes out visibly smaller than the rest; measured
/// by the mark, it matches them.
#[test]
fn a_mark_inside_a_halo_is_sized_by_the_mark() {
    // Retail: 570x440 trimmed, 431x346 of it solid.
    let ink = (431.0 * 346.0) / (570.0 * 440.0);
    let (haloed, _) = logo_size(570.0 / 440.0, ink);
    let (plain, _) = logo_size(570.0 / 440.0, 1.0);
    assert!(haloed > plain * 1.2, "{haloed} vs {plain}");

    // And what is left once the halo is discounted is the standard area.
    let (w, h) = logo_size(570.0 / 440.0, ink);
    let mark = (w * h) * ink;
    assert!((mark - LOGO_AREA).abs() / LOGO_AREA < 0.01, "{mark}");
}

/// A narrow mark is nudged in; a wide one is left where it is.
#[test]
fn only_the_narrow_marks_are_nudged_off_the_edge() {
    assert_eq!(logo_offset(LOGO_BOX_WIDTH), 0.0);
    assert_eq!(logo_offset(LOGO_ANCHOR_WIDTH), 0.0);
    // Forever, the narrowest, by half what it is short of the anchor.
    assert_eq!(logo_offset(263.0), (LOGO_ANCHOR_WIDTH - 263.0) / 2.0);
}

/// A reading of almost-all-halo would ask for a wordmark several times
/// everyone else's. The floor is what stops that.
#[test]
fn an_impossible_ink_reading_is_floored() {
    let (w, h) = logo_size(2.0, 0.01);
    let (floored, _) = logo_size(2.0, LOGO_MIN_INK);
    assert_eq!(w, floored);
    assert!(w <= LOGO_BOX_WIDTH && h <= LOGO_BOX_HEIGHT);
}
