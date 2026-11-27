//! simee's public website, drawn by Rune. Each page is this app in web document mode: the
//! browser scrolls a semantic mirror of it (real headings, paragraphs and links), and the canvas
//! draws the band in view. `--html` exports the same tree as each page's static first frame
//! (`scripts/build.sh`), which is what crawlers, readers without WebGPU and readers without
//! JavaScript get.
//!
//! The route comes from the page's path in the browser and from `SIMEE_WEB_ROUTE` when exporting
//! natively. Links between pages are ordinary links: each page is its own HTML file.

#![forbid(unsafe_code)]

use std::time::Duration;

use rune::{AppSpec, Cx, El, Image, ImageCache, SurfaceFit, Tx, col, el};

pub mod content;
mod demos;
mod devices;
mod docs;
mod gem;
mod home;
mod tokens;
mod ui;

use content::Doc;
use tokens::*;

/// The widths each page's static HTML is laid out at (`--html-widths`).
pub const EXPORT_WIDTHS: &str = tokens::Bp::EXPORT;

pub const TITLE: &str = "simee · iOS simulators and Android emulators from one Rust core";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Home,
    Docs,
    Doc(&'static Doc),
    Devices,
    Demos,
    NotFound,
}

impl PartialEq for Doc {
    fn eq(&self, other: &Self) -> bool {
        self.slug == other.slug
    }
}
impl Eq for Doc {}
impl std::fmt::Debug for Doc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.slug)
    }
}

impl Route {
    /// The page at `path` (`/`, `/docs/`, `/docs/android/`, `/devices/`, `/demos/`), with or
    /// without its trailing slash or `index.html`.
    pub fn parse(path: &str) -> Route {
        let path = path.trim_end_matches("index.html");
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        match parts.as_slice() {
            [] => Route::Home,
            ["docs"] => Route::Docs,
            ["docs", slug] => Doc::find(slug).map_or(Route::NotFound, Route::Doc),
            ["devices"] => Route::Devices,
            ["demos"] => Route::Demos,
            _ => Route::NotFound,
        }
    }

    /// Every page the site publishes, as (path, route): what `scripts/build.sh` exports.
    pub fn all() -> Vec<(String, Route)> {
        let mut all = vec![("/".to_owned(), Route::Home), ("/docs/".to_owned(), Route::Docs)];
        all.extend(content::DOCS.iter().map(|d| (d.href(), Route::Doc(d))));
        all.push(("/devices/".to_owned(), Route::Devices));
        all.push(("/demos/".to_owned(), Route::Demos));
        all
    }

    /// The page's `<title>`.
    pub fn title(&self) -> String {
        match self {
            Route::Home => TITLE.to_owned(),
            Route::Docs => "Docs · simee".to_owned(),
            Route::Doc(d) => format!("{} · simee docs", d.title()),
            Route::Devices => "Device concepts · simee".to_owned(),
            Route::Demos => "Demos · simee".to_owned(),
            Route::NotFound => "Not found · simee".to_owned(),
        }
    }

    /// The page's meta description.
    pub fn description(&self) -> String {
        match self {
            Route::Home | Route::NotFound => content::readme_lede(),
            Route::Docs => "simee's documentation: the comparison with the stock tools, benchmarks, the CLI, API and MCP server, both platforms' backends and the architecture.".to_owned(),
            Route::Doc(d) => d.summary(),
            Route::Devices => "Every form factor simee supports on iOS and Android, with each kind's reference profile and inputs.".to_owned(),
            Route::Demos => "The simee desktop window with iOS and Android devices, and how to run it.".to_owned(),
        }
    }
}

/// The route this process draws: the browser's path, or `SIMEE_WEB_ROUTE` when exporting.
fn current_route() -> Route {
    #[cfg(target_arch = "wasm32")]
    let path = web_sys::window().and_then(|w| w.location().pathname().ok()).unwrap_or_default();
    #[cfg(not(target_arch = "wasm32"))]
    let path = rune::env::var("SIMEE_WEB_ROUTE").unwrap_or_else(|| "/".to_owned());
    Route::parse(&path)
}

pub struct State {
    route: Route,
    images: Option<ImageCache>,
    /// The frame clock, in seconds: what turns the gem.
    t: f64,
}

impl State {
    fn new() -> State {
        State { route: current_route(), images: None, t: 0.0 }
    }

    #[cfg(test)]
    fn at(route: Route) -> State {
        State { route, images: None, t: 0.0 }
    }

    /// The image `docs/{name}` the site serves at `/img/{name}`. On the web it is fetched from
    /// the site's root, whatever page this is; exporting, it is read from the output folder
    /// (the build runs there) and the build makes the exported `src` absolute.
    fn image(&self, name: &str, pt: f32, scale: f32) -> Option<Image> {
        let cache = self.images.as_ref()?;
        #[cfg(target_arch = "wasm32")]
        return Some(cache.url(&format!("/img/{name}"), pt, scale));
        #[cfg(not(target_arch = "wasm32"))]
        return Some(cache.file(format!("img/{name}"), pt, scale));
    }
}

/// The gem's angle now: a turn every `gem::TURN` seconds on the home page, still elsewhere and
/// under reduced motion.
pub(crate) fn angle(s: &State, cx: &Cx<State>) -> f32 {
    const REST: f32 = 0.35;
    if s.route != Route::Home || cx.is_reduced_motion() {
        return REST;
    }
    REST + (s.t / gem::TURN * std::f64::consts::TAU) as f32
}

/// On the home page the gem turns: about 30 frames a second of a canvas, the rest memoized.
fn poll(s: &mut State, t: f64, tx: &mut Tx) -> bool {
    if s.route != Route::Home {
        return false;
    }
    s.t = t;
    tx.wake_after(Duration::from_millis(33));
    true
}

/// A screenshot from `docs/`, framed, holding its box at `aspect` before it arrives.
pub(crate) fn shot(s: &State, cx: &Cx<State>, name: &str, alt: &str, aspect: f32) -> El {
    let key = format!("shot-{name}");
    let shown = cx.layout_size(&key).map_or(MEASURE, |sz| sz[0]);
    let scale = cx.env().scale.max(1.0);
    let mut e = el().key(key).role("image").label(alt).w_full().aspect_ratio(aspect).border(1.0, rule_faint());
    if let Some(img) = s.image(name, shown, scale) {
        e = e.pixels_fit(img.pixels(), SurfaceFit::Fit);
    }
    e
}

fn view(s: &State, cx: &mut Cx<State>) -> El {
    let bp = cx.layout_with("page", |r| Bp::of(r[2])).unwrap_or(Bp { at: 1000 });
    let gutter = bp.gutter();
    let framed = |e: El| e.w_full().max_w(MEASURE + 2.0 * gutter).px(gutter);
    let main = match s.route {
        Route::Home => home::page(s, cx, bp),
        Route::Docs => docs::index(bp),
        Route::Doc(doc) => docs::page(doc, bp),
        Route::Devices => devices::page(bp),
        Route::Demos => demos::page(s, cx, bp),
        Route::NotFound => not_found(bp),
    };
    col()
        .key("page")
        .w_full()
        .fill(bg())
        .items_center()
        .child(framed(ui::topbar(&s.route, angle(s, cx))))
        .child(framed(col().key("main").role("main").child(main)))
        .child(framed(ui::footer()))
}

fn not_found(bp: Bp) -> El {
    col()
        .key("nf")
        .w_full()
        .pt(64.0)
        .gap(12.0)
        .child(rune::text("No page here.").key("nf-h").role("heading").level(1).font(bp.title()).color(fg()).wrap())
        .child(
            rune::row()
                .key("nf-links")
                .gap(18.0)
                .child(ui::link_text("nf-home", "Home", "/", BODY))
                .child(ui::link_text("nf-docs", "Docs", "/docs/", BODY)),
        )
}

pub fn spec() -> AppSpec<State> {
    // Rune sets the document's title from the spec: this page's, made once per page load.
    let title: &'static str = Box::leak(current_route().title().into_boxed_str());
    AppSpec {
        families: (SERIF, MONO),
        setup: Some(|s, waker| s.images = Some(ImageCache::new(waker))),
        poll: Some(poll),
        ..AppSpec::new(title, (1000.0, 900.0), State::new, view)
    }
}

/// CMU Serif's faces, served from `/fonts/` (`public/fonts/`) and read from `fonts/` beside the
/// export, which runs in the output folder.
const FONTS: [&str; 3] = ["cmunrm.otf", "cmunti.otf", "cmunbx.otf"];

/// The site as an app: document mode, so the browser owns scrolling, find and anchors.
pub fn app() -> rune::App<State> {
    let mut app = rune::App::new(spec()).web_document();
    for face in FONTS {
        let url = if cfg!(target_arch = "wasm32") { format!("/fonts/{face}") } else { format!("fonts/{face}") };
        app = app.font_url(&url);
    }
    app
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(start))]
pub fn start() {
    #[cfg(target_arch = "wasm32")]
    if let Err(error) = app().try_run() {
        log_error(&error.to_string());
    }
    #[cfg(not(target_arch = "wasm32"))]
    app().run();
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn log_error(message: &str);
}

#[cfg(test)]
mod tests;
