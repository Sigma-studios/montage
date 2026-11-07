use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use montage_core::edit::{Caption, Color, Cut, DEFAULT_FONT};
use montage_core::render::{Overlay, RgbaImage};

use crate::{CaptionOverlay, Fonts, SANS_SERIF, blend, caption_rect, render_caption};

const W: u32 = 1280;
const H: u32 = 720;

/// The system fonts, loaded once for the whole test binary.
fn shared() -> &'static Arc<Mutex<Fonts>> {
    static FONTS: OnceLock<Arc<Mutex<Fonts>>> = OnceLock::new();
    FONTS.get_or_init(|| {
        let start = Instant::now();
        let fonts = Fonts::load();
        eprintln!(
            "Fonts::load: {} families in {:?}",
            fonts.families().len(),
            start.elapsed()
        );
        Arc::new(Mutex::new(fonts))
    })
}

/// The fonts, or `None` inside a Nix build, whose sandbox has no fonts at
/// all. Anywhere else, fonts must be found.
fn fonts() -> Option<MutexGuard<'static, Fonts>> {
    let fonts = shared().lock().unwrap_or_else(|e| e.into_inner());
    if fonts.families().is_empty() {
        assert!(
            std::env::var_os("NIX_BUILD_TOP").is_some(),
            "no fonts found outside a Nix build"
        );
        eprintln!("no fonts in this sandbox; skipping");
        return None;
    }
    Some(fonts)
}

/// A caption in a font that's installed here (Impact if it is).
fn caption(fonts: &Fonts, text: &str) -> Caption {
    Caption {
        text: text.into(),
        font: fonts.resolve_family(DEFAULT_FONT).0.to_owned(),
        ..Caption::default()
    }
}

fn pixel(image: &RgbaImage, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * image.width + x) * 4) as usize;
    image.data[i..i + 4].try_into().unwrap()
}

fn opaque(color: [u8; 4]) -> RgbaImage {
    RgbaImage {
        width: 1,
        height: 1,
        data: color.to_vec(),
    }
}

/// The box around the pixels with some alpha, as (x0, y0, x1, y1), the end
/// exclusive.
fn ink_box(image: &RgbaImage) -> Option<(u32, u32, u32, u32)> {
    let mut b: Option<(u32, u32, u32, u32)> = None;
    for y in 0..image.height {
        for x in 0..image.width {
            if pixel(image, x, y)[3] > 0 {
                let (x0, y0, x1, y1) = b.unwrap_or((x, y, x + 1, y + 1));
                b = Some((x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1)));
            }
        }
    }
    b
}

/// How many pixels of row `y` have some alpha.
fn ink_in_row(image: &RgbaImage, y: u32) -> usize {
    (0..image.width)
        .filter(|&x| pixel(image, x, y)[3] > 0)
        .count()
}

#[test]
fn types_are_send() {
    fn send<T: Send>() {}
    send::<Fonts>();
    send::<CaptionOverlay>();
}

#[test]
fn families_are_sorted_and_unique() {
    let Some(fonts) = fonts() else { return };
    let families = fonts.families();
    assert!(!families.is_empty());
    for pair in families.windows(2) {
        let (a, b) = (pair[0].to_lowercase(), pair[1].to_lowercase());
        assert!(a < b, "{:?} then {:?}", pair[0], pair[1]);
    }
    assert!(families.iter().all(|f| fonts.has_family(f)));
    // Case doesn't matter.
    let first = &families[0];
    assert!(fonts.has_family(&first.to_uppercase()));
    assert_eq!(
        fonts.resolve_family(&first.to_uppercase()),
        (&**first, false)
    );
}

#[test]
fn missing_fonts_fall_back_to_impact_then_sans_serif() {
    let Some(mut fonts) = fonts() else { return };
    let missing = "No Such Font Montage 1234";
    assert!(!fonts.has_family(missing));
    let (family, fell_back) = fonts.resolve_family(missing);
    assert!(fell_back);
    if fonts.has_family(DEFAULT_FONT) {
        assert!(family.eq_ignore_ascii_case(DEFAULT_FONT));
    } else {
        let sans = SANS_SERIF.iter().find(|f| fonts.has_family(f));
        match sans {
            Some(sans) => assert!(family.eq_ignore_ascii_case(sans), "{family}"),
            None => assert_eq!(family, fonts.families()[0]),
        }
    }
    let family = family.to_owned();

    let mut c = caption(&fonts, "gg");
    let found = render_caption(&mut fonts, &c, W, H);
    assert!(!found.fell_back);
    c.font = missing.into();
    let missed = render_caption(&mut fonts, &c, W, H);
    assert!(missed.fell_back);
    assert_eq!(missed.family, family);
    assert!(ink_box(&missed.image).is_some(), "the fallback draws");
}

#[test]
fn caption_is_centred_white_with_a_black_outline() {
    let Some(mut fonts) = fonts() else { return };
    let c = caption(&fonts, "Hello world");
    let start = Instant::now();
    let r = render_caption(&mut fonts, &c, W, H);
    eprintln!("render_caption: {:?}", start.elapsed());
    let image = &r.image;
    assert!(image.width > 50 && image.height > 20, "{image:?}");
    assert_eq!(image.data.len(), (image.width * image.height * 4) as usize);

    // Horizontally on x = 0.5, vertically on y = 0.12 (the capitals' middle).
    let size = 0.1 * H as f32;
    let cx = r.x as f32 + image.width as f32 / 2.0;
    let cy = r.y as f32 + image.height as f32 / 2.0;
    assert!(
        (cx - 0.5 * W as f32).abs() < 0.02 * W as f32,
        "centre x {cx}"
    );
    assert!((cy - 0.12 * H as f32).abs() < 0.2 * size, "centre y {cy}");
    // The box is tight, outline included: ink comes within a pixel or two
    // of each side (the box has a pixel to spare for anti-aliasing).
    let (x0, y0, x1, y1) = ink_box(image).unwrap();
    assert!(x0 <= 2 && y0 <= 2, "{:?}", (x0, y0));
    assert!(x1 + 2 >= image.width && y1 + 2 >= image.height);

    // Each row starts and ends with the outline, and the inside is white.
    let mut white = 0;
    for y in 0..image.height {
        let inked: Vec<[u8; 4]> = (0..image.width)
            .map(|x| pixel(image, x, y))
            .filter(|p| p[3] > 0)
            .collect();
        if let (Some(first), Some(last)) = (inked.first(), inked.last()) {
            for edge in [first, last] {
                assert!(edge[..3].iter().all(|&c| c < 60), "row {y}: {edge:?}");
            }
        }
        white += inked
            .iter()
            .filter(|p| p[3] == 255 && p[..3].iter().all(|&c| c > 240))
            .count();
    }
    assert!(white > 500, "{white} white pixels");
    let black = image
        .data
        .chunks_exact(4)
        .filter(|p| p[3] == 255 && p[..3].iter().all(|&c| c < 20))
        .count();
    assert!(black > 500, "{black} black pixels");
}

#[test]
fn glyphs_are_drawn_upright() {
    let Some(mut fonts) = fonts() else { return };
    let mut c = caption(&fonts, "T");
    c.outline_width = 0.0;
    let r = render_caption(&mut fonts, &c, W, H);
    let (_, y0, _, y1) = ink_box(&r.image).unwrap();
    let quarter = (y1 - y0) / 4;
    // The bar of the T is at the top.
    assert!(ink_in_row(&r.image, y0 + quarter / 2) > 2 * ink_in_row(&r.image, y1 - quarter));
}

#[test]
fn capitals_are_applied() {
    let Some(mut fonts) = fonts() else { return };
    let lower = caption(&fonts, "nice shot");
    let upper = Caption {
        text: "NICE SHOT".into(),
        uppercase: false,
        ..lower.clone()
    };
    let as_typed = Caption {
        uppercase: false,
        ..lower.clone()
    };
    let lower = render_caption(&mut fonts, &lower, W, H);
    let upper = render_caption(&mut fonts, &upper, W, H);
    let as_typed = render_caption(&mut fonts, &as_typed, W, H);
    assert_eq!((lower.x, lower.y), (upper.x, upper.y));
    assert_eq!(lower.image, upper.image);
    assert_ne!(lower.image, as_typed.image);
}

#[test]
fn size_scales_the_caption() {
    let Some(mut fonts) = fonts() else { return };
    let small = caption(&fonts, "GG");
    let big = Caption {
        size: 0.2,
        ..small.clone()
    };
    let small = render_caption(&mut fonts, &small, W, H);
    let big = render_caption(&mut fonts, &big, W, H);
    let ratio = big.image.height as f32 / small.image.height as f32;
    assert!((1.7..2.3).contains(&ratio), "{ratio}");
    assert!(big.image.width > small.image.width);
}

#[test]
fn long_text_wraps_within_the_frame() {
    let Some(mut fonts) = fonts() else { return };
    let one = caption(&fonts, "when you hit the shot");
    let long = caption(
        &fonts,
        "when you finally hit the shot you have been practising for three whole weeks and nobody saw it",
    );
    let one = render_caption(&mut fonts, &one, W, H);
    let long = render_caption(&mut fonts, &long, W, H);
    let line = 0.1 * H as f32;
    assert!(one.image.height < (1.5 * line) as u32);
    assert!(
        long.image.height as f32 > one.image.height as f32 + line,
        "{}",
        long.image.height
    );
    assert!(long.image.width <= W);
    assert!(long.x >= 0 && long.x as u32 + long.image.width <= W);

    // Explicit newlines start a line.
    let broken = caption(&fonts, "gg\nwp");
    let broken = render_caption(&mut fonts, &broken, W, H);
    assert!(broken.image.height as f32 > one.image.height as f32 + 0.8 * line);
    assert!(broken.image.width < one.image.width);
}

#[test]
fn shadow_falls_down_and_right() {
    let Some(mut fonts) = fonts() else { return };
    let plain = caption(&fonts, "GG");
    let shadowed = Caption {
        shadow: true,
        ..plain.clone()
    };
    let plain = render_caption(&mut fonts, &plain, W, H);
    let shadowed = render_caption(&mut fonts, &shadowed, W, H);
    assert_eq!((plain.x, plain.y), (shadowed.x, shadowed.y));
    assert!(shadowed.image.width > plain.image.width);
    assert!(shadowed.image.height > plain.image.height);
    // Translucent black in the bottom-right corner area.
    let (w, h) = (shadowed.image.width, shadowed.image.height);
    let corner: Vec<[u8; 4]> = (h * 3 / 4..h)
        .flat_map(|y| (w * 3 / 4..w).map(move |x| (x, y)))
        .map(|(x, y)| pixel(&shadowed.image, x, y))
        .filter(|p| p[3] > 0 && p[3] < 200 && p[..3] == [0, 0, 0])
        .collect();
    assert!(!corner.is_empty());
}

#[test]
fn empty_captions_draw_nothing() {
    let Some(mut fonts) = fonts() else { return };
    let c = caption(&fonts, "  \n ");
    let r = render_caption(&mut fonts, &c, W, H);
    assert_eq!((r.image.width, r.image.height), (1, 1));
    assert_eq!(r.image.data, [0; 4]);
    assert_eq!((r.x, r.y), (640, 86));
}

#[test]
fn caption_rect_matches_the_render() {
    let Some(mut fonts) = fonts() else { return };
    let c = Caption {
        x: 0.3,
        y: 0.8,
        shadow: true,
        ..caption(&fonts, "bottom text")
    };
    let r = render_caption(&mut fonts, &c, W, H);
    let rect = caption_rect(&mut fonts, &c, W, H);
    assert_eq!(rect, (r.x, r.y, r.image.width, r.image.height));
    assert!(r.contains(r.x, r.y));
    assert!(!r.contains(r.x - 1, r.y));
    assert!(!r.contains(r.x + r.image.width as i32, r.y));
}

#[test]
fn labels_are_tight_and_coloured() {
    let Some(mut fonts) = fonts() else { return };
    let families: Vec<String> = fonts.families().iter().take(40).cloned().collect();
    let red = Color::rgb(255, 0, 0);
    let start = Instant::now();
    for family in &families {
        let label = fonts.render_label(family, family, 18.0, red);
        assert!(label.width >= 1 && label.height >= 1);
    }
    eprintln!(
        "render_label: {:?} per family",
        start.elapsed() / families.len() as u32
    );

    let family = fonts.resolve_family(DEFAULT_FONT).0.to_owned();
    let label = fonts.render_label(&family, "Impact", 24.0, red);
    assert!(label.height > 10 && label.height < 40, "{label:?}");
    assert!(label.width > label.height);
    // Tight to the pixel: an edge pixel may hold too little of a curve to
    // show.
    let (x0, y0, x1, y1) = ink_box(&label).unwrap();
    assert!(x0 <= 1 && y0 <= 1 && x1 + 1 >= label.width && y1 + 1 >= label.height);
    let solid = label
        .data
        .chunks_exact(4)
        .filter(|p| p[3] == 255)
        .collect::<Vec<_>>();
    assert!(!solid.is_empty());
    assert!(solid.iter().all(|p| p[..3] == [255, 0, 0]));

    let missing = fonts.render_label("No Such Font Montage 1234", "abc", 24.0, red);
    assert!(ink_box(&missing).is_some());
    let empty = fonts.render_label(&family, "", 24.0, red);
    assert_eq!((empty.width, empty.height, empty.data), (1, 1, vec![0; 4]));
}

#[test]
fn blend_mixes_by_alpha() {
    let blue = [0, 0, 255, 255];
    let cases = [
        ([255, 0, 0, 0], blue),
        ([255, 0, 0, 255], [255, 0, 0, 255]),
        ([255, 0, 0, 128], [128, 0, 127, 255]),
        ([255, 255, 255, 51], [51, 51, 255, 255]),
    ];
    for (src, want) in cases {
        let mut frame = opaque(blue);
        blend(&mut frame, &opaque(src), 0, 0);
        assert_eq!(frame.data, want, "{src:?}");
    }
    // Translucent on translucent: the general "over".
    let mut frame = opaque([0, 0, 0, 128]);
    blend(&mut frame, &opaque([255, 255, 255, 128]), 0, 0);
    assert_eq!(frame.data, [170, 170, 170, 192]);
    let mut frame = opaque([10, 20, 30, 0]);
    blend(&mut frame, &opaque([200, 100, 50, 77]), 0, 0);
    assert_eq!(frame.data, [200, 100, 50, 77]);
}

#[test]
fn blend_clips_at_the_edges() {
    // A 3x3 white square onto a 4x4 black frame.
    let white = RgbaImage {
        width: 3,
        height: 3,
        data: [255; 36].to_vec(),
    };
    let frame = RgbaImage {
        width: 4,
        height: 4,
        data: [0, 0, 0, 255].repeat(16),
    };
    let lit = |frame: &RgbaImage| -> Vec<(u32, u32)> {
        (0..16)
            .map(|i| (i % 4, i / 4))
            .filter(|&(x, y)| pixel(frame, x, y)[0] == 255)
            .collect()
    };
    let mut f = frame.clone();
    blend(&mut f, &white, -2, -1);
    assert_eq!(lit(&f), [(0, 0), (0, 1)]);
    let mut f = frame.clone();
    blend(&mut f, &white, 3, 2);
    assert_eq!(lit(&f), [(3, 2), (3, 3)]);
    for (x, y) in [(-3, 0), (4, 0), (0, -3), (0, 4), (i32::MIN, i32::MAX)] {
        let mut f = frame.clone();
        blend(&mut f, &white, x, y);
        assert_eq!(f, frame, "{x}, {y}");
    }
    let mut f = frame.clone();
    blend(&mut f, &white, 1, 1);
    assert_eq!(lit(&f).len(), 9);
}

#[test]
fn overlay_draws_within_the_time_range_and_reuses_renders() {
    let Some(fonts) = fonts() else { return };
    let timed = Caption {
        start: Some(1.0),
        end: Some(2.0),
        ..caption(&fonts, "GG")
    };
    let blank = caption(&fonts, " ");
    drop(fonts);
    let cut = Cut {
        start: 0.0,
        end: 10.0,
    };

    let empty = CaptionOverlay::new(shared().clone(), vec![blank.clone()], cut);
    assert!(empty.is_empty());
    let mut overlay = CaptionOverlay::new(shared().clone(), vec![timed, blank], cut);
    assert!(!overlay.is_empty());

    let black = |w, h| RgbaImage {
        width: w,
        height: h,
        data: [0, 0, 0, 255].repeat((w * h) as usize),
    };
    for t in [0.0, 0.99, 2.0, 5.0] {
        let mut frame = black(W, H);
        overlay.draw(t, &mut frame);
        assert_eq!(frame, black(W, H), "at {t}");
    }
    assert_eq!(overlay.renders, 0);

    let mut first = black(W, H);
    overlay.draw(1.0, &mut first);
    assert_ne!(first, black(W, H));
    let start = Instant::now();
    for t in [1.25, 1.5, 1.99] {
        let mut frame = black(W, H);
        overlay.draw(t, &mut frame);
        assert_eq!(frame, first, "at {t}");
    }
    eprintln!("cached overlay draw: {:?}", start.elapsed() / 3);
    assert_eq!(overlay.renders, 1);

    // A new frame size draws again.
    let mut small = black(640, 360);
    overlay.draw(1.5, &mut small);
    assert_ne!(small, black(640, 360));
    assert_eq!(overlay.renders, 2);
}
