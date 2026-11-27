//! Demos: the desktop window, as figures.

use rune::{Cx, El, col, text};

use crate::tokens::*;
use crate::ui::{centered, link_text};
use crate::{State, shot};

pub fn page(s: &State, cx: &Cx<State>, bp: Bp) -> El {
    let figure = |n: &str, name: &str, alt: &str, cap: &str| {
        col()
            .key(format!("fig{n}"))
            .role("figure")
            .w_full()
            .mt(28.0)
            .child(shot(s, cx, name, alt, 2480.0 / 1740.0))
            .child(centered(&format!("fig{n}-cap"), &format!("Figure {n}. {cap}"), SMALL, fg2()).mt(8.0))
    };
    col()
        .key("demos")
        .w_full()
        .pt(if bp.phone() { 28.0 } else { 44.0 })
        .child(text("Demos").key("demos-h1").role("heading").level(1).font(bp.title()).color(fg()).wrap())
        .child(figure(
            "1",
            "desktop.png",
            "The desktop window with iOS and Android devices",
            "iOS and Android devices in one window, with the iOS debug panel.",
        ))
        .child(figure(
            "2",
            "desktop-android.png",
            "The desktop window with an Android device",
            "An Android device on emu, simee's emulator.",
        ))
        .child(link_text("demos-more", "The desktop window's controls and shortcuts", "/docs/desktop/", BODY).mt(24.0))
}
