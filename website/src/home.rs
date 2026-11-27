//! The home page, set as a paper: title block with Figure 1 (the turning gem), abstract, then the
//! measured comparison and the debug surface, quick start, limitations and references. Every
//! table and number is the README's or a doc's own text; the page adds captions only.

use rune::{Cx, El, col, el, memo, text};

use crate::content::{self, REPO};
use crate::gem;
use crate::tokens::*;
use crate::ui::{caption, centered, link_text, markdown, prose, section};
use crate::{State, angle};

/// The README sections the page shows.
pub const README_SECTIONS: [&str; 3] = ["Compared with the stock tools", "Quick start", "Not yet"];

/// The debug surface rows Table 3 (the README's iOS comparison) doesn't already cover.
const MORE_DEBUG: [&str; 5] = ["Logs", "In-device exec", "Device state", "Events", "Launch"];

pub fn page(s: &State, cx: &Cx<State>, bp: Bp) -> El {
    let width = cx.layout_size("main").map_or(MEASURE, |sz| sz[0]);
    col().key("home").w_full().child(title_block(angle(s, cx), width, bp)).child(memo("body", bp, || body(bp)))
}

fn title_block(angle: f32, width: f32, bp: Bp) -> El {
    col()
        .key("title-block")
        .w_full()
        .items_center()
        .pt(if bp.phone() { 28.0 } else { 44.0 })
        .child(
            centered("title", "simee: iOS Simulators and Android Emulators from One Rust Core", bp.title(), fg()).role("heading").level(1),
        )
        .child(link_text("byline", "github.com/kaeawc/simee", REPO, SMALL).mt(10.0))
        .child(
            col()
                .key("fig1")
                .role("figure")
                .items_center()
                .mt(20.0)
                .child(gem::figure(angle, width))
                .child(centered("fig1-cap", "Figure 1. The simee mark, drawn and turned by this page's Rune app.", SMALL, fg2()).mt(4.0)),
        )
}

fn body(bp: Bp) -> El {
    let compared = content::subsections(&content::readme_section(README_SECTIONS[0]));
    let table = |i: usize, n: &str| -> El {
        let (heading, body) = compared.get(i).cloned().unwrap_or_default();
        col()
            .key(format!("t{n}"))
            .w_full()
            .child(caption(&format!("t{n}-cap"), &format!("Table {n}"), &heading.replace('`', "")))
            .child(markdown(&format!("t{n}-md"), &content::until(&body, "How each number")))
    };
    let debug = content::doc_section("debugging", "iOS debug surface");
    // debugging.md's one paragraph on Android, after the iOS table.
    let android_debug = debug.find("Android devices").map(|at| debug[at..].split("\n\n").next().unwrap_or("").replace('\n', " "));
    let mut debugging = section("debugging", "2", "Debugging", bp)
        .child(table(2, "3"))
        .child(caption("t4-cap", "Table 4", "More of the iOS debug surface."))
        .child(markdown("t4-md", &content::table_rows(&debug, &MORE_DEBUG)));
    if let Some(android) = android_debug {
        debugging = debugging.child(prose("android-debug", &android, BODY, fg()).mt(14.0));
    }
    let quick = content::readme_section(README_SECTIONS[1]);
    let not_yet = content::readme_section(README_SECTIONS[2]);
    col()
        .key("paper")
        .w_full()
        .max_w(MEASURE)
        .self_center()
        .child(abstract_(bp))
        .child(section("performance", "1", "Performance", bp).child(table(0, "1")).child(table(1, "2")))
        .child(debugging)
        .child(section("quick-start", "3", "Quick start", bp).child(markdown("qs-md", strip_heading(&quick))))
        .child(section("limitations", "4", "Limitations", bp).child(markdown("lim-md", strip_heading(&not_yet))))
        .child(references(bp))
}

fn strip_heading(md: &str) -> &str {
    md.split_once('\n').map_or("", |(_, rest)| rest)
}

fn abstract_(bp: Bp) -> El {
    let inset = if bp.phone() { 0.0 } else { 36.0 };
    col()
        .key("abstract")
        .role("region")
        .label("Abstract")
        .w_full()
        .px(inset)
        .pt(28.0)
        .child(centered("abstract-h", "Abstract", BOLD, fg()).mb(6.0))
        .child(prose("abstract-p", &content::readme_lede_code(), BODY, fg()))
}

fn references(bp: Bp) -> El {
    let refs: [(&str, &str); 6] = [
        ("Benchmarks: every published number and the command that reruns it", "/docs/benchmarks/"),
        ("Compared with the stock tools", "/docs/comparison/"),
        ("Debugging: the iOS debug surface, network shaping, audio, Android counters", "/docs/debugging/"),
        ("The Android backend: emu", "/docs/android/"),
        ("Form factors and their support matrix", "/docs/form-factors/"),
        ("Source code (MIT)", REPO),
    ];
    let mut list = col().key("refs-list").role("list").gap(4.0);
    for (i, (label, href)) in refs.iter().enumerate() {
        list = list.child(
            el().key(format!("ref-{i}")).role("listitem").w_full().child(
                rune::row()
                    .key(format!("ref-{i}-row"))
                    .gap(10.0)
                    .items_start()
                    .child(text(format!("[{}]", i + 1)).font(BODY).color(fg2()).w(30.0))
                    .child(link_text(&format!("ref-{i}-a"), label, href, BODY).w(0.0).grow(1.0)),
            ),
        );
    }
    section("references", "", "References", bp).child(list)
}
