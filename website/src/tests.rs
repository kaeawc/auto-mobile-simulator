use rune::{Env, Runtime};

use crate::content::{self, DOCS, Doc, block_keys, outline, resolve_links, site_url, slug};
use crate::tokens::Bp;
use crate::{Route, State, view};

fn page(route: Route, width: f32) -> Runtime<State> {
    let mut rt = Runtime::new(State::at(route), view, (width, 900.0), Env::new(true, 2.0));
    for _ in 0..3 {
        rt.frame(0.0);
    }
    rt
}

fn paths(rt: &Runtime<State>) -> Vec<String> {
    rt.elements().into_iter().filter(|e| !e.exiting).map(|e| e.path).collect()
}

/// The static page keeps the narrowest width's tree and switches styles with `@media`, so every
/// width a page is exported at must build the same elements.
#[test]
fn every_page_builds_the_same_tree_at_every_breakpoint() {
    let widths: Vec<f32> = Bp::EXPORT.split(',').map(|w| w.parse().unwrap()).chain([1400.0]).collect();
    for (path, route) in Route::all().into_iter().chain([("/404".to_owned(), Route::NotFound)]) {
        let narrow = paths(&page(route, widths[0]));
        assert!(narrow.iter().any(|p| p.ends_with("main")), "{path} has no main: {narrow:?}");
        for &w in &widths[1..] {
            let wide = paths(&page(route, w));
            let only: Vec<_> = wide.iter().filter(|p| !narrow.contains(p)).chain(narrow.iter().filter(|p| !wide.contains(p))).collect();
            assert!(only.is_empty(), "{path} at {w}: {only:?}");
        }
    }
}

#[test]
fn routes_parse_with_and_without_slashes() {
    assert_eq!(Route::parse("/"), Route::Home);
    assert_eq!(Route::parse("/index.html"), Route::Home);
    assert_eq!(Route::parse("/docs"), Route::Docs);
    assert_eq!(Route::parse("/docs/android/"), Route::Doc(Doc::find("android").unwrap()));
    assert_eq!(Route::parse("/docs/android/index.html"), Route::Doc(Doc::find("android").unwrap()));
    assert_eq!(Route::parse("/docs/research/"), Route::NotFound);
    assert_eq!(Route::parse("/devices/"), Route::Devices);
    assert_eq!(Route::parse("/nope"), Route::NotFound);
}

#[test]
fn every_published_route_parses_back_to_itself() {
    for (path, route) in Route::all() {
        assert_eq!(Route::parse(&path), route, "{path}");
    }
}

#[test]
fn every_doc_has_a_title_and_a_summary() {
    for doc in DOCS {
        assert_ne!(doc.title(), doc.slug, "{} has no # title", doc.slug);
        assert!(!doc.summary().is_empty(), "{} has no first paragraph", doc.slug);
    }
}

#[test]
fn links_resolve_to_site_pages_images_and_github() {
    assert_eq!(site_url("android.md#performance-notes", "docs"), "/docs/android/#performance-notes");
    assert_eq!(site_url("docs/benchmarks.md#frame-pacing", ""), "/docs/benchmarks/#frame-pacing");
    assert_eq!(site_url("desktop.png", "docs"), "/img/desktop.png");
    assert_eq!(site_url("docs/desktop.png", ""), "/img/desktop.png");
    assert_eq!(site_url("../README.md", "docs"), "/");
    assert_eq!(site_url("research/bluetooth.md", "docs"), "https://github.com/kaeawc/simee/blob/main/docs/research/bluetooth.md");
    assert_eq!(
        site_url("../crates/sim-core/src/form_factor.rs", "docs"),
        "https://github.com/kaeawc/simee/blob/main/crates/sim-core/src/form_factor.rs"
    );
    assert_eq!(site_url("https://example.com/a.md", "docs"), "https://example.com/a.md");
    assert_eq!(site_url("#toolsets", "docs"), "#toolsets");
}

#[test]
fn links_in_code_are_left_alone() {
    let md = "See [cli](cli.md) and `[x](cli.md)`.\n```\n[y](cli.md)\n```\n";
    assert_eq!(resolve_links(md, "docs"), "See [cli](/docs/cli/) and `[x](cli.md)`.\n```\n[y](cli.md)\n```\n");
}

#[test]
fn no_published_doc_links_to_a_relative_path() {
    // Every relative link in a published doc becomes a site page, a site image or GitHub.
    for doc in DOCS {
        let body = doc.body();
        for (i, _) in body.match_indices("](") {
            let target = &body[i + 2..];
            let target = &target[..target.find(')').unwrap_or(target.len())];
            let ok = target.starts_with('/')
                || target.starts_with('#')
                || target.contains("://")
                || target.starts_with("mailto:")
                || target.contains('`')
                || target.contains(' ');
            assert!(ok || body[..i].matches("```").count() % 2 == 1, "{}: {target}", doc.slug);
        }
    }
}

#[test]
fn anchors_match_github() {
    assert_eq!(slug("Performance notes"), "performance-notes");
    assert_eq!(slug("Boot to `sys.boot_completed` with emu's devices"), "boot-to-sysboot_completed-with-emus-devices");
    assert_eq!(slug("API 29 and 30: first boots and idle host CPU"), "api-29-and-30-first-boots-and-idle-host-cpu");
    let keys = block_keys("# A\n\npara\n\n## B\n\n- x\n\n## B\n\n```\n## not\n```\n");
    assert_eq!(keys, [(0, "a".to_owned()), (11, "b".to_owned()), (22, "b-1".to_owned())]);
    assert_eq!(outline("## One\n\ntext\n\n## Two `x`\n"), [("one".to_owned(), "One".to_owned()), ("two-x".to_owned(), "Two x".to_owned())]);
}

#[test]
fn the_readme_sections_the_home_page_shows_exist() {
    for heading in crate::home::README_SECTIONS.into_iter().chain(["Compared with the stock tools", "Not yet"]) {
        let md = content::readme_section(heading);
        assert!(md.lines().count() > 2, "README has no ## {heading}");
    }
    assert!(content::readme_lede().starts_with("iOS simulators and Android emulators"));
    // The abstract is prose, never the README's HTML (its `<img>` mark showed as text).
    assert!(content::readme_lede_code().starts_with("iOS simulators and Android emulators"), "{}", content::readme_lede_code());
}

#[test]
fn themes_cycle_and_round_trip() {
    use crate::theme::Theme;
    let mut t = Theme::System;
    for want in [Theme::Light, Theme::Dark, Theme::System] {
        t = t.next();
        assert_eq!(t, want);
        assert_eq!(Theme::parse(t.as_str()), t);
    }
    assert!(Theme::System.dark(true) && !Theme::System.dark(false));
    assert!(Theme::Dark.dark(false) && !Theme::Light.dark(true));
}

#[test]
fn code_spans_naming_repo_paths_link_to_them() {
    let md = "rerun with `scripts/bench/android-boot.sh`, see `docs/cli.md`, not `simee list` or [`scripts/x.sh`](https://example.com/y)";
    let out = resolve_links(md, "");
    assert!(
        out.contains("[`scripts/bench/android-boot.sh`](https://github.com/kaeawc/simee/blob/main/scripts/bench/android-boot.sh)"),
        "{out}"
    );
    assert!(out.contains("[`docs/cli.md`](/docs/cli/)"), "{out}");
    assert!(out.contains("`simee list`") && out.contains("[`scripts/x.sh`](https://example.com/y)"), "{out}");
    // From a doc, the path is still the repository's.
    assert!(resolve_links("`scripts/lint.sh`", "docs").contains("(https://github.com/kaeawc/simee/blob/main/scripts/lint.sh)"));
}

#[test]
fn quick_start_drops_its_table() {
    let quick = content::without_tables(&content::readme_section("Quick start"));
    assert!(!quick.lines().any(|l| l.starts_with('|')), "{quick}");
    assert!(quick.contains("simee doctor"), "{quick}");
}
