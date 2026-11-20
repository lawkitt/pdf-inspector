//! Text a reader never sees although it is painted: runs drawn in the
//! colour of what lies under them (white on the unpainted page, white on a
//! white fill, red on a red band) and runs a later opaque paint covers.
//!
//! The page walker logs what the page paints, in order, as boxes in device
//! space: solid fills of a known colour, opaque images, and everything else
//! as an *unknown* region, which may hold any colour. A run is hidden when
//!
//! - every colour it paints with is within [`SAME_COLOUR_TOLERANCE`] of the
//!   colour under its whole box: the topmost earlier opaque solid fill that
//!   holds the box, with every paint above that fill in the box's way of the
//!   run's colour too, or the white page when nothing earlier is painted
//!   there; or
//! - a later opaque solid fill or opaque image holds its whole box.
//!
//! Form XObjects are walked into the same log, cut by the clip they are
//! invoked under and their bounding box; a form that is not walked to its
//! end paints an unknown region over its box.
//!
//! A run shown in optional content that the document's default
//! configuration turns off (see `optional_content`) is hidden whatever its
//! colour or what lies under it: an open `/OC` marked-content sequence
//! whose properties are off, or a form carrying an off `/OC`, invoked in
//! off content or inside such a form. Paints in optional content, off or
//! not, stay unknown regions, as below.
//!
//! Conservative by construction: when in doubt the run stays. An image, a
//! shading, an inline image, a non-rectangular fill, a fill cut by a clip
//! of unknown shape, anything in optional content, and any paint whose
//! blending, soft mask, overprint or colour space is not read is an unknown
//! region, so text over it keeps whatever its colour; such a region never
//! covers either. Only upright runs shown in an understood colour, outside
//! optional content, in a font that is not Type3 (whose glyphs may paint in
//! their own colours), are judged by colour; a run whose advance is an
//! estimate is judged with a box twice that wide. Strokes are lines: they
//! are neither a background nor a cover. Pages that paint more than
//! [`MAX_PAINTS`] fills judge nothing.

use std::collections::HashMap;

use lopdf::{Dictionary, Document, Object, ObjectId};

use super::clip_boundaries::{ClipBounds, ClipTracker};
use super::get_number;
use super::optional_content::{owner_off, OffLayers};
use super::text_paint::RunPaint;
use crate::types::{ItemType, TextItem};

/// Colours within this distance on every 8-bit sRGB channel (about 2.4 %)
/// are the same colour: 0.98 gray (250) on white is hidden, 0.96 gray (245)
/// is pale but readable and stays. Wide enough for the rounding of colour
/// conversions, narrow enough that no colour a reader can tell from its
/// background is ever hidden.
pub(crate) const SAME_COLOUR_TOLERANCE: u8 = 6;

/// The most paints a page's log holds. A page painting more (dense charts,
/// maps) judges nothing: a paint missing from the log could be the dark
/// background some white text sits on.
const MAX_PAINTS: usize = 10_000;

/// The most subpaths one fill may have for its rectangles to be read one by
/// one; a fill with more is an unknown region.
const MAX_SUBPATHS: usize = 64;

/// The unpainted page.
const PAPER: [u8; 3] = [255, 255, 255];

/// Slack, in points, when one box must hold another.
const EPSILON: f32 = 0.01;

/// An axis-aligned box in device space.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Area {
    left: f32,
    bottom: f32,
    right: f32,
    top: f32,
}

impl Area {
    const EVERYWHERE: Area = Area {
        left: f32::NEG_INFINITY,
        bottom: f32::NEG_INFINITY,
        right: f32::INFINITY,
        top: f32::INFINITY,
    };

    /// The box around `points`; `None` for no points or a non-finite one.
    fn around(points: impl IntoIterator<Item = (f32, f32)>) -> Option<Area> {
        let mut area: Option<Area> = None;
        for (x, y) in points {
            if !x.is_finite() || !y.is_finite() {
                return None;
            }
            let a = area.get_or_insert(Area {
                left: x,
                bottom: y,
                right: x,
                top: y,
            });
            a.left = a.left.min(x);
            a.right = a.right.max(x);
            a.bottom = a.bottom.min(y);
            a.top = a.top.max(y);
        }
        area
    }

    fn from_bounds([left, bottom, right, top]: [f32; 4]) -> Area {
        Area {
            left,
            bottom,
            right,
            top,
        }
    }

    /// The common part, when it has an area.
    fn intersection(self, other: Area) -> Option<Area> {
        let area = Area {
            left: self.left.max(other.left),
            bottom: self.bottom.max(other.bottom),
            right: self.right.min(other.right),
            top: self.top.min(other.top),
        };
        (area.left < area.right && area.bottom < area.top).then_some(area)
    }

    fn overlaps(self, other: Area) -> bool {
        self.intersection(other).is_some()
    }

    fn contains(self, other: Area) -> bool {
        self.left <= other.left + EPSILON
            && self.bottom <= other.bottom + EPSILON
            && self.right >= other.right - EPSILON
            && self.top >= other.top - EPSILON
    }
}

/// One thing the page painted.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Paint {
    /// `colour` filling exactly `area`, normally blended; `opaque` when at
    /// full alpha.
    Solid {
        area: Area,
        colour: [u8; 3],
        opaque: bool,
    },
    /// An opaque image filling exactly `area`, normally blended; its pixels
    /// are not read.
    Image { area: Area },
    /// `colour`, normally blended, painted somewhere inside `area` in a
    /// shape that is not read: a stroke, a fill cut by a clip of unknown
    /// shape, a fill in optional content. Under a run of that colour it
    /// changes nothing; under any other it keeps the run.
    Scattered { area: Area, colour: [u8; 3] },
    /// Something painted somewhere inside `area` whose shape, colour or
    /// compositing is not read.
    Unknown { area: Area },
}

impl Paint {
    fn area(&self) -> Area {
        match *self {
            Paint::Solid { area, .. }
            | Paint::Image { area }
            | Paint::Scattered { area, .. }
            | Paint::Unknown { area } => area,
        }
    }

    /// Whether this paint hides whatever was painted in `area` before it.
    fn covers(&self, area: Area) -> bool {
        match *self {
            Paint::Solid {
                area: own,
                opaque: true,
                ..
            }
            | Paint::Image { area: own } => own.contains(area),
            _ => false,
        }
    }
}

/// The compositing entries of the graphics state that decide whether a
/// paint shows its own colour: each `true` while known to be the opaque
/// default.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Compositing {
    /// Fill alpha (`ca`) is 1.
    full_alpha: bool,
    /// Blend mode (`BM`) is Normal or Compatible.
    normal_blend: bool,
    /// Soft mask (`SMask`) is None.
    no_soft_mask: bool,
    /// Fill overprint (`op`, or `OP` without it) is off.
    no_overprint: bool,
}

impl Default for Compositing {
    fn default() -> Self {
        Compositing {
            full_alpha: true,
            normal_blend: true,
            no_soft_mask: true,
            no_overprint: true,
        }
    }
}

impl Compositing {
    const UNKNOWN: Compositing = Compositing {
        full_alpha: false,
        normal_blend: false,
        no_soft_mask: false,
        no_overprint: false,
    };

    /// Whether a paint shows its own colour where it lands, at whatever
    /// alpha: a fill blended normally, unmasked, without overprint.
    fn keeps_colour(self) -> bool {
        self.normal_blend && self.no_soft_mask && self.no_overprint
    }
}

/// What a `gs` operator changes: each entry the state dictionary sets, as
/// the [`Compositing`] flag it leaves.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct StateUpdate {
    full_alpha: Option<bool>,
    normal_blend: Option<bool>,
    no_soft_mask: Option<bool>,
    no_overprint: Option<bool>,
}

impl StateUpdate {
    fn read(doc: &Document, state: &Dictionary) -> StateUpdate {
        let get = |key: &[u8]| state.get(key).ok().and_then(|o| resolve(doc, o));
        let normal = |name: &[u8]| matches!(name, b"Normal" | b"Compatible");
        StateUpdate {
            full_alpha: get(b"ca").map(|o| get_number(o).is_some_and(|a| a >= 0.999)),
            normal_blend: get(b"BM").map(|o| match o {
                Object::Name(name) => normal(name),
                // An array lists modes in order of preference; a reader
                // uses the first it supports, and every reader supports
                // Normal.
                Object::Array(modes) => modes
                    .first()
                    .and_then(|m| m.as_name().ok())
                    .is_some_and(normal),
                _ => false,
            }),
            no_soft_mask: get(b"SMask").map(|o| o.as_name().ok() == Some(b"None")),
            no_overprint: get(b"op")
                .or_else(|| get(b"OP"))
                .map(|o| o.as_bool().ok() == Some(false)),
        }
    }

    fn apply(self, to: &mut Compositing) {
        let set = |flag: &mut bool, value: Option<bool>| {
            if let Some(value) = value {
                *flag = value;
            }
        };
        set(&mut to.full_alpha, self.full_alpha);
        set(&mut to.normal_blend, self.normal_blend);
        set(&mut to.no_soft_mask, self.no_soft_mask);
        set(&mut to.no_overprint, self.no_overprint);
    }
}

/// An XObject a page may invoke.
#[derive(Clone, Copy, Debug, PartialEq)]
enum XObject {
    /// An image; `opaque` when it has no soft mask, mask or stencil.
    /// `optional` when it carries optional content (`/OC`), which may be
    /// off.
    Image { opaque: bool, optional: bool },
    /// A form, with its bounding box and matrix; `off` when its optional
    /// content is off in the default configuration.
    Form {
        bbox: Option<[f32; 4]>,
        matrix: [f32; 6],
        optional: bool,
        off: bool,
    },
    /// Anything else.
    Other,
}

impl XObject {
    fn read(doc: &Document, layers: &OffLayers, obj: &Object) -> XObject {
        let Some(dict) = resolve(doc, obj).and_then(|o| match o {
            Object::Stream(s) => Some(&s.dict),
            Object::Dictionary(d) => Some(d),
            _ => None,
        }) else {
            return XObject::Other;
        };
        let get = |key: &[u8]| dict.get(key).ok().and_then(|o| resolve(doc, o));
        let numbers = |o: &Object| -> Option<Vec<f32>> {
            o.as_array()
                .ok()?
                .iter()
                .map(|v| get_number(resolve(doc, v)?))
                .collect()
        };
        let optional = get(b"OC").is_some();
        match get(b"Subtype").and_then(|o| o.as_name().ok()) {
            Some(b"Image") => {
                let stencil = get(b"ImageMask").is_some_and(|o| o.as_bool().ok() == Some(true));
                let masked = get(b"SMask").is_some_and(|o| o.as_name().ok() != Some(b"None"))
                    || get(b"Mask").is_some()
                    || get(b"SMaskInData").is_some_and(|o| o.as_i64().ok() != Some(0));
                XObject::Image {
                    opaque: !stencil && !masked,
                    optional,
                }
            }
            Some(b"Form") => {
                let bbox = get(b"BBox")
                    .and_then(numbers)
                    .and_then(|v| <[f32; 4]>::try_from(v).ok());
                let matrix = get(b"Matrix")
                    .and_then(numbers)
                    .and_then(|v| <[f32; 6]>::try_from(v).ok())
                    .unwrap_or([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
                XObject::Form {
                    bbox,
                    matrix,
                    optional,
                    off: owner_off(doc, layers, dict),
                }
            }
            _ => XObject::Other,
        }
    }
}

fn resolve<'a>(doc: &'a Document, mut obj: &'a Object) -> Option<&'a Object> {
    for _ in 0..8 {
        match obj {
            Object::Reference(id) => obj = doc.get_object(*id).ok()?,
            _ => return Some(obj),
        }
    }
    None
}

fn transform((x, y): (f32, f32), m: &[f32; 6]) -> (f32, f32) {
    (x * m[0] + y * m[2] + m[4], x * m[1] + y * m[3] + m[5])
}

/// `first` then `second`: the matrix of a form drawn under a CTM.
fn concat(first: &[f32; 6], second: &[f32; 6]) -> [f32; 6] {
    [
        first[0] * second[0] + first[1] * second[2],
        first[0] * second[1] + first[1] * second[3],
        first[2] * second[0] + first[3] * second[2],
        first[2] * second[1] + first[3] * second[3],
        first[4] * second[0] + first[5] * second[2] + second[4],
        first[4] * second[1] + first[5] * second[3] + second[5],
    ]
}

/// The four corners of `[x0, y0, x1, y1]` under `m`.
fn corners([x0, y0, x1, y1]: [f32; 4], m: &[f32; 6]) -> [(f32, f32); 4] {
    [
        transform((x0, y0), m),
        transform((x1, y0), m),
        transform((x1, y1), m),
        transform((x0, y1), m),
    ]
}

/// The box a closed run of points outlines when it is an axis-aligned
/// rectangle: four corners (a fifth repeating the first is dropped), each
/// edge horizontal or vertical, turning between the two.
fn rectangle(points: &[(f32, f32)]) -> Option<Area> {
    let mut points = points;
    if points.len() == 5 && points[0] == points[4] {
        points = &points[..4];
    }
    let [a, b, c, d] = <[(f32, f32); 4]>::try_from(points).ok()?;
    let level = |p: (f32, f32), q: (f32, f32)| (p.1 - q.1).abs() <= EPSILON;
    let plumb = |p: (f32, f32), q: (f32, f32)| (p.0 - q.0).abs() <= EPSILON;
    let starts_level = level(a, b) && plumb(b, c) && level(c, d) && plumb(d, a);
    let starts_plumb = plumb(a, b) && level(b, c) && plumb(c, d) && level(d, a);
    if !(starts_level || starts_plumb) {
        return None;
    }
    let area = Area::around([a, b, c, d])?;
    (area.left < area.right && area.bottom < area.top).then_some(area)
}

#[derive(Clone, Default)]
struct Subpath {
    points: Vec<(f32, f32)>,
    curved: bool,
}

/// What a text run paints with, judged when it is shown.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct ShownRun {
    /// How many paints came before the run.
    seq: usize,
    /// The colours the run paints with (fill, stroke or both), `None` when
    /// the run is not judged by colour.
    ink: Option<([u8; 3], Option<[u8; 3]>)>,
    /// Shown in optional content the default configuration turns off: no
    /// reader shows it, whatever its colour.
    off: bool,
}

/// The glyph runs shown inside an ActualText span, so that the replacement
/// text the span emits at its end is judged as its glyphs were.
#[derive(Clone, Copy, Debug)]
pub(super) struct SpanRuns {
    first: Option<ShownRun>,
    /// Every glyph run so far was shown like the first: the same colours,
    /// with nothing painted between them.
    alike: bool,
}

impl Default for SpanRuns {
    fn default() -> Self {
        SpanRuns {
            first: None,
            alike: true,
        }
    }
}

impl SpanRuns {
    /// A glyph run shown inside the span.
    pub(super) fn glyph(&mut self, run: ShownRun) {
        match self.first {
            None => self.first = Some(run),
            Some(first) => self.alike &= first == run,
        }
    }

    /// How the span's replacement, emitted now, is shown: as its glyph runs
    /// were when they were all alike and nothing has been painted since;
    /// otherwise unjudged, and off when it ends in off optional content or
    /// its glyph runs were all off.
    pub(super) fn close(self, log: &PaintLog<'_>) -> ShownRun {
        match self.first {
            Some(first) if self.alike && first.seq == log.paints.len() => first,
            Some(first) if self.alike && first.off => ShownRun {
                off: true,
                ..log.unjudged()
            },
            _ => log.unjudged(),
        }
    }
}

type States = HashMap<Vec<u8>, Option<StateUpdate>>;
type XObjects = HashMap<Vec<u8>, XObject>;
/// Per `/Properties` resource name, whether it is optional content the
/// default configuration turns off.
type Properties = HashMap<Vec<u8>, bool>;

/// An open marked-content sequence.
#[derive(Clone, Copy, Debug)]
struct Marked {
    /// Tagged `/OC`: optional content, which may be off.
    optional: bool,
    /// Optional content the default configuration turns off.
    off: bool,
}

/// A form XObject being walked: its own clip, and what to restore when it
/// ends, as a `Do` restores the graphics state it was invoked in.
struct FormFrame {
    clips: ClipTracker,
    /// Where the form may paint: its bounding box under its matrix.
    area: Area,
    compositing: Compositing,
    line_width: f32,
    saved_len: usize,
    marked_len: usize,
    path: Vec<Subpath>,
    /// The invoking stream's resources, when the form has its own.
    resources: Option<(States, XObjects, Properties)>,
    /// Whether what the form paints may not land as painted: it was
    /// invoked at less than full alpha (a transparency group's alpha
    /// applies to everything in it however its contents set their own), in
    /// optional content, or inside such a form.
    inexact: bool,
    /// Whether what the form paints may not keep its colour: it was
    /// invoked blended, soft-masked or overprinted, or inside such a form.
    uncoloured: bool,
    /// Whether the form is off: it carries optional content the default
    /// configuration turns off, was invoked in such content, or inside
    /// such a form.
    off: bool,
}

/// A form invoked and not yet entered: where it may paint, the clip it was
/// invoked under, and whether it carries optional content, and off.
#[derive(Clone, Copy, Debug)]
struct PendingForm {
    area: Area,
    clip: ClipBounds,
    optional: bool,
    off: bool,
}

/// What a page has painted so far, and the graphics state that decides how
/// its next paint lands.
pub(super) struct PaintLog<'a> {
    /// The document, to read optional content a marked-content sequence
    /// carries inline; `None` for a detached log.
    doc: Option<&'a Document>,
    /// The optional content the document's default configuration turns
    /// off.
    layers: OffLayers,
    paints: Vec<Paint>,
    overflowed: bool,
    compositing: Compositing,
    /// The line width in user space (`w`).
    line_width: f32,
    saved: Vec<(Compositing, f32)>,
    /// The open marked-content sequences.
    marked: Vec<Marked>,
    path: Vec<Subpath>,
    states: States,
    xobjects: XObjects,
    properties: Properties,
    /// A form XObject just invoked, with the clip it was invoked under: the
    /// walker enters it next, or it was not walked and paints somewhere in
    /// its area.
    pending_form: Option<PendingForm>,
    forms: Vec<FormFrame>,
}

impl<'a> PaintLog<'a> {
    /// An empty log reading `gs`, `Do` and `BDC` operands from the page's
    /// resources, its own before inherited ones.
    pub(super) fn page(doc: &'a Document, page_id: ObjectId) -> PaintLog<'a> {
        let mut log = PaintLog {
            doc: Some(doc),
            layers: OffLayers::read(doc),
            paints: Vec::new(),
            overflowed: false,
            compositing: Compositing::default(),
            line_width: 1.0,
            saved: Vec::new(),
            marked: Vec::new(),
            path: Vec::new(),
            states: HashMap::new(),
            xobjects: HashMap::new(),
            properties: HashMap::new(),
            pending_form: None,
            forms: Vec::new(),
        };
        if let Ok((own, inherited)) = doc.get_page_resources(page_id) {
            let inherited = inherited
                .into_iter()
                .filter_map(|id| doc.get_dictionary(id).ok());
            for resources in own.into_iter().chain(inherited) {
                log.add_resources(doc, resources);
            }
        }
        log
    }

    fn add_resources(&mut self, doc: &Document, resources: &Dictionary) {
        let section = |key: &[u8]| {
            resources
                .get(key)
                .ok()
                .and_then(|o| resolve(doc, o))
                .and_then(|o| o.as_dict().ok())
        };
        if let Some(states) = section(b"ExtGState") {
            for (name, obj) in states {
                let update = resolve(doc, obj)
                    .and_then(|o| o.as_dict().ok())
                    .map(|state| StateUpdate::read(doc, state));
                self.states.entry(name.clone()).or_insert(update);
            }
        }
        if let Some(xobjects) = section(b"XObject") {
            for (name, obj) in xobjects {
                self.xobjects
                    .entry(name.clone())
                    .or_insert_with(|| XObject::read(doc, &self.layers, obj));
            }
        }
        if let Some(properties) = section(b"Properties") {
            for (name, obj) in properties {
                let off = self.layers.hides(doc, obj);
                self.properties.entry(name.clone()).or_insert(off);
            }
        }
    }

    /// A log for a form walked on its own, outside any page: nothing it
    /// paints is judged.
    #[cfg(test)]
    pub(super) fn detached() -> PaintLog<'static> {
        PaintLog {
            doc: None,
            layers: OffLayers::None,
            paints: Vec::new(),
            overflowed: true,
            compositing: Compositing::default(),
            line_width: 1.0,
            saved: Vec::new(),
            marked: Vec::new(),
            path: Vec::new(),
            states: HashMap::new(),
            xobjects: HashMap::new(),
            properties: HashMap::new(),
            pending_form: None,
            forms: Vec::new(),
        }
    }

    /// Start walking the form XObject `form` (its stream dictionary) just
    /// invoked, `ctm` its matrix under the invoking CTM. Its paints are cut
    /// by the clip it was invoked under and its bounding box; its `gs` and
    /// `Do` operands name its own resources, or the invoking stream's when
    /// it has none.
    pub(super) fn enter_form(&mut self, doc: &Document, form: &Dictionary, ctm: &[f32; 6]) {
        let PendingForm {
            area,
            clip,
            optional,
            off,
        } = self.pending_form.take().unwrap_or(PendingForm {
            area: Area::EVERYWHERE,
            clip: ClipBounds::Unknown,
            optional: false,
            off: false,
        });
        let off = off || self.off() || owner_off(doc, &self.layers, form);
        let mut clips = ClipTracker::starting_with(clip);
        let bbox = form
            .get(b"BBox")
            .ok()
            .and_then(|o| resolve(doc, o))
            .and_then(|o| o.as_array().ok())
            .and_then(|a| {
                a.iter()
                    .map(|v| resolve(doc, v).and_then(get_number))
                    .collect::<Option<Vec<f32>>>()
            })
            .and_then(|v| <[f32; 4]>::try_from(v).ok());
        match bbox {
            Some([x0, y0, x1, y1]) => {
                let operands =
                    [x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs()].map(Object::Real);
                clips.observe("re", &operands, *ctm);
            }
            // No box: the clip is not known.
            None => clips.observe("m", &[], *ctm),
        }
        clips.observe("W", &[], *ctm);
        clips.observe("n", &[], *ctm);
        let resources = form
            .get(b"Resources")
            .ok()
            .and_then(|o| resolve(doc, o))
            .and_then(|o| o.as_dict().ok())
            .map(|own| {
                let outer = (
                    std::mem::take(&mut self.states),
                    std::mem::take(&mut self.xobjects),
                    std::mem::take(&mut self.properties),
                );
                self.add_resources(doc, own);
                outer
            });
        let inexact = self.inexact()
            || optional
            || !self.compositing.full_alpha
            || self.forms.last().is_some_and(|frame| frame.inexact);
        let uncoloured = !self.compositing.keeps_colour()
            || self.forms.last().is_some_and(|frame| frame.uncoloured);
        self.forms.push(FormFrame {
            clips,
            area,
            compositing: self.compositing,
            line_width: self.line_width,
            saved_len: self.saved.len(),
            marked_len: self.marked.len(),
            path: std::mem::take(&mut self.path),
            resources,
            inexact,
            uncoloured,
            off,
        });
    }

    /// Finish the form [`enter_form`](Self::enter_form) started. A walk
    /// that stopped short (`walked_all` false) left paints unread: the form
    /// may have painted anything anywhere in its area.
    pub(super) fn leave_form(&mut self, walked_all: bool) {
        let Some(frame) = self.forms.pop() else {
            return;
        };
        self.compositing = frame.compositing;
        self.line_width = frame.line_width;
        self.saved.truncate(frame.saved_len);
        self.marked.truncate(frame.marked_len);
        self.path = frame.path;
        if let Some((states, xobjects, properties)) = frame.resources {
            self.states = states;
            self.xobjects = xobjects;
            self.properties = properties;
        }
        if !walked_all {
            self.record(frame.area, Kind::Unknown, ClipBounds::None);
        }
    }

    /// Follow one operator of the form being walked, `ctm` and `paint` as
    /// for [`observe`](Self::observe); the clip is the form's own.
    pub(super) fn observe_in_form(
        &mut self,
        operator: &str,
        operands: &[Object],
        ctm: &[f32; 6],
        paint: RunPaint,
    ) {
        let clip = self
            .forms
            .last()
            .map_or(ClipBounds::Unknown, |frame| frame.clips.bounds());
        self.observe(operator, operands, ctm, clip, paint);
        if let Some(frame) = self.forms.last_mut() {
            frame.clips.observe(operator, operands, *ctm);
        }
    }

    /// Settle a form invoked last that was never walked: it paints
    /// somewhere in its area.
    fn settle_pending_form(&mut self) {
        if let Some(form) = self.pending_form.take() {
            self.record(form.area, Kind::Unknown, form.clip);
        }
    }

    /// Settle what the stream left open, before judging runs.
    pub(super) fn finish(&mut self) {
        self.settle_pending_form();
    }

    fn in_optional_content(&self) -> bool {
        self.marked.iter().any(|marked| marked.optional)
    }

    /// Whether what is shown now is in optional content the default
    /// configuration turns off: an open marked-content sequence or the form
    /// being walked is off.
    fn off(&self) -> bool {
        self.marked.iter().any(|marked| marked.off)
            || self.forms.last().is_some_and(|frame| frame.off)
    }

    /// Whether a paint now may not land as painted (see `FormFrame`).
    fn inexact(&self) -> bool {
        self.in_optional_content() || self.forms.last().is_some_and(|frame| frame.inexact)
    }

    /// Whether a paint now may not keep its colour.
    fn uncoloured(&self) -> bool {
        !self.compositing.keeps_colour() || self.forms.last().is_some_and(|frame| frame.uncoloured)
    }

    /// The most a stroke of the current line width reaches past its path
    /// in device space: the width itself (twice the half width a butt or
    /// round stroke reaches, for joins and caps), at least a point for a
    /// hairline.
    fn stroke_reach(&self, ctm: &[f32; 6]) -> f32 {
        let scale = (ctm[0].abs() + ctm[2].abs()).max(ctm[1].abs() + ctm[3].abs());
        let reach = self.line_width.abs() * scale;
        if reach.is_finite() {
            reach.max(1.0)
        } else {
            f32::INFINITY
        }
    }

    /// Follow one content-stream operator: `ctm` and `clip` as they stand
    /// before it applies, `paint` the current fill and stroke colours.
    pub(super) fn observe(
        &mut self,
        operator: &str,
        operands: &[Object],
        ctm: &[f32; 6],
        clip: ClipBounds,
        paint: RunPaint,
    ) {
        self.settle_pending_form();
        let fill = paint.fill_color;
        let numbers = || -> Option<Vec<f32>> { operands.iter().map(get_number).collect() };
        match operator {
            "q" => self.saved.push((self.compositing, self.line_width)),
            "Q" => {
                // A form's unbalanced `Q` cannot reach the invoking stream's
                // saved states.
                let floor = self.forms.last().map_or(0, |frame| frame.saved_len);
                (self.compositing, self.line_width) = if self.saved.len() > floor {
                    self.saved
                        .pop()
                        .unwrap_or((Compositing::UNKNOWN, f32::INFINITY))
                } else {
                    (Compositing::UNKNOWN, f32::INFINITY)
                };
            }
            "w" => {
                self.line_width = operands
                    .first()
                    .and_then(get_number)
                    .unwrap_or(f32::INFINITY);
            }
            "gs" => {
                match operands
                    .first()
                    .and_then(|o| o.as_name().ok())
                    .and_then(|name| self.states.get(name).copied().flatten())
                {
                    Some(update) => update.apply(&mut self.compositing),
                    None => self.compositing = Compositing::UNKNOWN,
                }
            }
            "BDC" => {
                let optional = operands
                    .first()
                    .and_then(|o| o.as_name().ok())
                    .is_some_and(|tag| tag == b"OC");
                let off = optional
                    && match operands.get(1) {
                        Some(Object::Name(name)) => {
                            self.properties.get(name).copied().unwrap_or(false)
                        }
                        Some(inline) => self.doc.is_some_and(|doc| self.layers.hides(doc, inline)),
                        None => false,
                    };
                self.marked.push(Marked { optional, off });
            }
            "BMC" => self.marked.push(Marked {
                optional: false,
                off: false,
            }),
            "EMC" => {
                let floor = self.forms.last().map_or(0, |frame| frame.marked_len);
                if self.marked.len() > floor {
                    self.marked.pop();
                }
            }
            "m" => match numbers().as_deref() {
                Some(&[x, y]) => self.path.push(Subpath {
                    points: vec![transform((x, y), ctm)],
                    curved: false,
                }),
                _ => self.break_path(),
            },
            "l" | "c" | "v" | "y" => {
                let Some(values) = numbers().filter(|v| {
                    v.len()
                        == match operator {
                            "l" => 2,
                            "c" => 6,
                            _ => 4,
                        }
                }) else {
                    return self.break_path();
                };
                let Some(subpath) = self.path.last_mut() else {
                    return self.break_path();
                };
                subpath.curved |= operator != "l";
                subpath.points.extend(
                    values
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|&[x, y]| transform((x, y), ctm)),
                );
            }
            "re" => match numbers().as_deref() {
                Some(&[x, y, w, h]) => self.path.push(Subpath {
                    points: corners([x, y, x + w, y + h], ctm).to_vec(),
                    curved: false,
                }),
                _ => self.break_path(),
            },
            "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" | "S" | "s" => {
                let path = std::mem::take(&mut self.path);
                if !matches!(operator, "S" | "s") {
                    for (area, exact) in Self::filled_areas(&path) {
                        let kind = if exact {
                            Kind::Solid(fill)
                        } else {
                            Kind::Scattered(fill)
                        };
                        self.record(area, kind, clip);
                    }
                }
                if matches!(operator, "B" | "B*" | "b" | "b*" | "S" | "s") && !path.is_empty() {
                    // A stroke paints along its path, as wide as the line.
                    let reach = self.stroke_reach(ctm);
                    let area = Area::around(path.iter().flat_map(|s| s.points.iter().copied()))
                        .map(|a| Area {
                            left: a.left - reach,
                            bottom: a.bottom - reach,
                            right: a.right + reach,
                            top: a.top + reach,
                        })
                        .unwrap_or(Area::EVERYWHERE);
                    self.record(area, Kind::Scattered(paint.stroke_color), clip);
                }
            }
            "n" => self.path.clear(),
            "sh" => {
                let area = match clip {
                    ClipBounds::Rect(bounds) => Area::from_bounds(bounds),
                    _ => Area::EVERYWHERE,
                };
                self.record(area, Kind::Unknown, clip);
            }
            "Do" => {
                let xobject = operands
                    .first()
                    .and_then(|o| o.as_name().ok())
                    .and_then(|name| self.xobjects.get(name).copied())
                    .unwrap_or(XObject::Other);
                match xobject {
                    XObject::Image { opaque, optional } => {
                        let opaque = opaque && !optional;
                        let points = corners([0.0, 0.0, 1.0, 1.0], ctm);
                        match (rectangle(&points), Area::around(points)) {
                            (Some(area), _) => self.record(area, Kind::Image(opaque), clip),
                            (None, Some(area)) => self.record(area, Kind::Unknown, clip),
                            (None, None) => self.record(Area::EVERYWHERE, Kind::Unknown, clip),
                        }
                    }
                    // The walker enters the form next and logs what it
                    // paints; a form it does not walk is settled as an
                    // unknown region by the next operator.
                    XObject::Form {
                        bbox,
                        matrix,
                        optional,
                        off,
                    } => {
                        let area = bbox
                            .and_then(|bbox| Area::around(corners(bbox, &concat(&matrix, ctm))))
                            .unwrap_or(Area::EVERYWHERE);
                        self.pending_form = Some(PendingForm {
                            area,
                            clip,
                            optional,
                            off,
                        });
                    }
                    _ => self.record(Area::EVERYWHERE, Kind::Unknown, clip),
                }
            }
            // An inline image, however the parser reports it.
            "BI" | "ID" | "EI" => {
                let area =
                    Area::around(corners([0.0, 0.0, 1.0, 1.0], ctm)).unwrap_or(Area::EVERYWHERE);
                self.record(area, Kind::Unknown, clip);
            }
            _ => {}
        }
    }

    /// A path whose construction went wrong: whatever it fills is unknown.
    fn break_path(&mut self) {
        self.path.push(Subpath {
            points: vec![(f32::NAN, f32::NAN)],
            curved: true,
        });
    }

    /// The areas a fill of `path` paints, each with whether it paints that
    /// area exactly. Rectangles that do not overlap fill each of their own
    /// boxes under either fill rule; anything else paints somewhere inside
    /// the box around its points, or anywhere when that box is unknown.
    fn filled_areas(path: &[Subpath]) -> Vec<(Area, bool)> {
        if path.is_empty() {
            return Vec::new();
        }
        let points = || path.iter().flat_map(|s| s.points.iter().copied());
        let unknown = || vec![(Area::around(points()).unwrap_or(Area::EVERYWHERE), false)];
        if path.len() > MAX_SUBPATHS {
            return unknown();
        }
        let rects: Option<Vec<Area>> = path
            .iter()
            .map(|s| (!s.curved).then(|| rectangle(&s.points)).flatten())
            .collect();
        let Some(rects) = rects else {
            return unknown();
        };
        let apart = rects
            .iter()
            .enumerate()
            .all(|(i, a)| rects[i + 1..].iter().all(|b| !a.overlaps(*b)));
        if !apart {
            return unknown();
        }
        rects.into_iter().map(|area| (area, true)).collect()
    }

    fn record(&mut self, area: Area, kind: Kind, clip: ClipBounds) {
        let (area, exact) = match clip {
            ClipBounds::None => (area, true),
            ClipBounds::Rect(bounds) => match area.intersection(Area::from_bounds(bounds)) {
                Some(area) => (area, true),
                None => return,
            },
            ClipBounds::Unknown => (area, false),
        };
        let exact = exact && !self.inexact();
        let coloured = !self.uncoloured();
        let paint = match kind {
            Kind::Solid(Some(colour)) if exact && coloured => Paint::Solid {
                area,
                colour,
                opaque: self.compositing.full_alpha,
            },
            Kind::Solid(Some(colour)) | Kind::Scattered(Some(colour)) if coloured => {
                Paint::Scattered { area, colour }
            }
            Kind::Image(true) if exact && coloured && self.compositing.full_alpha => {
                Paint::Image { area }
            }
            _ => Paint::Unknown { area },
        };
        if self.paints.len() < MAX_PAINTS {
            self.paints.push(paint);
        } else {
            self.overflowed = true;
        }
    }

    /// A run shown now with `paint`, judged by colour only when it is
    /// shown outside optional content and outside any form that may not
    /// land as painted, blended normally, in a font that is not Type3, with
    /// every colour it paints in known.
    pub(super) fn shown_run(&self, paint: RunPaint, type3: bool) -> ShownRun {
        let judged = !type3 && !self.inexact() && !self.uncoloured();
        let ink = match paint.render_mode {
            0 | 4 => paint.fill_color.map(|fill| (fill, None)),
            1 | 5 => paint.stroke_color.map(|stroke| (stroke, None)),
            2 | 6 => paint
                .fill_color
                .zip(paint.stroke_color)
                .map(|(fill, stroke)| (fill, Some(stroke))),
            _ => None,
        };
        ShownRun {
            seq: self.paints.len(),
            ink: ink.filter(|_| judged),
            off: self.off(),
        }
    }

    /// Something other than a text run shown now (a form's text, an
    /// ActualText replacement, an image placeholder): never judged by
    /// colour, still covered by what comes later, and off in optional
    /// content the default configuration turns off.
    pub(super) fn unjudged(&self) -> ShownRun {
        ShownRun {
            seq: self.paints.len(),
            ink: None,
            off: self.off() || self.pending_form.is_some_and(|form| form.off),
        }
    }

    /// Whether `item`, shown as `run`, is hidden (see the module docs). A
    /// text run in off optional content is hidden wherever it lies.
    pub(super) fn hides(&self, item: &TextItem, run: &ShownRun) -> bool {
        if run.off && matches!(item.item_type, ItemType::Text) && !item.text.trim().is_empty() {
            return true;
        }
        if self.overflowed
            || !matches!(item.item_type, ItemType::Text)
            || item.text.trim().is_empty()
            || !(item.width > 0.0 && item.height > 0.0)
            || ![item.x, item.y, item.width, item.height]
                .iter()
                .all(|v| v.is_finite())
        {
            return false;
        }
        let turn = item.rotation.rem_euclid(360.0);
        if turn > 0.5 && turn < 359.5 {
            return false;
        }
        // The run's box from its baseline up one em, widened for
        // descenders and for glyphs overhanging their advance. A run whose
        // advance is an estimate (half an em a glyph) gets twice that
        // width, centred on it: a wider box only makes a run harder to call
        // hidden, under a fill or under a cover.
        let h = item.height;
        let slack = if item.advance_known {
            0.0
        } else {
            item.width / 2.0
        };
        let ink_box = Area {
            left: item.x - 0.1 * h - slack,
            bottom: item.y - 0.25 * h,
            right: item.x + item.width + 0.1 * h + slack,
            top: item.y + h,
        };
        let seq = run.seq.min(self.paints.len());
        let covered = self.paints[seq..].iter().any(|p| p.covers(ink_box));
        covered
            || run
                .ink
                .is_some_and(|ink| self.same_colour_beneath(ink_box, seq, ink))
    }

    fn same_colour_beneath(
        &self,
        ink_box: Area,
        seq: usize,
        (first, second): ([u8; 3], Option<[u8; 3]>),
    ) -> bool {
        let matches = |colour: [u8; 3]| {
            same_colour(first, colour) && second.is_none_or(|s| same_colour(s, colour))
        };
        for paint in self.paints[..seq].iter().rev() {
            if !paint.area().overlaps(ink_box) {
                continue;
            }
            match *paint {
                Paint::Solid {
                    area,
                    colour,
                    opaque,
                } if matches(colour) => {
                    if opaque && area.contains(ink_box) {
                        return true;
                    }
                }
                Paint::Scattered { colour, .. } if matches(colour) => {}
                _ => return false,
            }
        }
        matches(PAPER)
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    /// A fill of exactly the area, in this colour when known.
    Solid(Option<[u8; 3]>),
    /// Paint of this colour, when known, somewhere in the area.
    Scattered(Option<[u8; 3]>),
    /// An image of exactly the area; `true` when opaque.
    Image(bool),
    Unknown,
}

/// Whether two colours are within [`SAME_COLOUR_TOLERANCE`] on every
/// channel.
pub(crate) fn same_colour(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| x.abs_diff(*y) <= SAME_COLOUR_TOLERANCE)
}

/// What [`drop_hidden_runs`] left out: runs, and the stretches they make.
///
/// A stretch is hidden text with nothing kept between its parts, as for
/// invisible text (`invisible::InvisibleText`): consecutive hidden runs, in
/// the order the page shows them, count once, and any item kept between
/// them (visible text, an image placeholder) starts a new stretch. Runs of
/// spaces only are never hidden and neither count nor split a stretch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HiddenText {
    pub(crate) runs: u32,
    pub(crate) stretches: u32,
}

/// Remove from `items` the runs `log` judges hidden (each shown as the
/// matching entry of `runs`), keeping the vectors parallel to it (`clips`,
/// `replaced_text`, `coverage`) aligned. A run its clip leaves out is left
/// to `drop_clipped_away_runs` and not counted here: it is not on the page
/// at all. With `off_only`, only runs in off optional content are hidden:
/// a walk that keeps invisible text to read a scan's text layer keeps the
/// runs judged by colour, but no layer turned off is a scan's text layer.
pub(super) fn drop_hidden_runs(
    items: &mut Vec<TextItem>,
    clips: &mut Vec<Option<super::clip_boundaries::ClipRect>>,
    replaced_text: &mut Vec<bool>,
    coverage: &mut crate::types::ItemCoverage,
    runs: &[ShownRun],
    log: &PaintLog<'_>,
    off_only: bool,
) -> HiddenText {
    debug_assert_eq!(items.len(), runs.len());
    let mut hidden = HiddenText::default();
    let mut open = false;
    let keep: Vec<bool> = items
        .iter()
        .zip(runs)
        .zip(clips.iter())
        .map(|((item, run), clip)| {
            // A run its clip leaves out is not on the page: it is dropped
            // later and splits nothing.
            if super::clip_boundaries::excluded_by_clip(item, *clip) {
                return true;
            }
            let hides = if off_only {
                run.off && matches!(item.item_type, ItemType::Text) && !item.text.trim().is_empty()
            } else {
                log.hides(item, run)
            };
            if !hides {
                let blank = matches!(item.item_type, ItemType::Text) && item.text.trim().is_empty();
                open &= blank;
                return true;
            }
            hidden.runs += 1;
            if !open {
                hidden.stretches += 1;
                open = true;
            }
            log::trace!(
                "hidden run left out: {:?} at ({}, {}) {}x{}",
                item.text,
                item.x,
                item.y,
                item.width,
                item.height
            );
            false
        })
        .collect();
    if hidden.runs == 0 {
        return hidden;
    }
    let mut kept = keep.iter();
    items.retain(|_| *kept.next().unwrap_or(&true));
    let mut kept = keep.iter();
    clips.retain(|_| *kept.next().unwrap_or(&true));
    let mut kept = keep.iter();
    replaced_text.retain(|_| *kept.next().unwrap_or(&true));
    let mut kept = keep.iter();
    coverage.retain(|_| *kept.next().unwrap_or(&true));
    hidden
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::content::Content;

    const IDENTITY: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

    /// Paint filling in `fill` and stroking in black.
    fn fill_paint(fill: [u8; 3]) -> RunPaint {
        RunPaint {
            fill_color: Some(fill),
            stroke_color: Some([0, 0, 0]),
            render_mode: 0,
        }
    }

    fn empty_log() -> PaintLog<'static> {
        PaintLog {
            overflowed: false,
            ..PaintLog::detached()
        }
    }

    /// The log after `ops`, all at the identity CTM with no clip, filling
    /// in `fill`.
    fn log_after(ops: &str, fill: [u8; 3]) -> PaintLog<'static> {
        let mut log = empty_log();
        for op in Content::decode(ops.as_bytes()).unwrap().operations {
            log.observe(
                &op.operator,
                &op.operands,
                &IDENTITY,
                ClipBounds::None,
                fill_paint(fill),
            );
        }
        log
    }

    fn area(left: f32, bottom: f32, right: f32, top: f32) -> Area {
        Area {
            left,
            bottom,
            right,
            top,
        }
    }

    fn run(text: &str, x: f32, y: f32, width: f32) -> TextItem {
        TextItem {
            text: text.into(),
            x,
            y,
            width,
            height: 12.0,
            font: "Helvetica".into(),
            font_tag: "F1".into(),
            legacy_symbol_rewrite: false,
            font_size: 12.0,
            page: 1,
            is_bold: false,
            is_italic: false,
            font_weight: None,
            bold_source: None,
            fixed_pitch: None,
            fill_color: None,
            stroke_color: None,
            render_mode: None,
            is_underline: false,
            is_strikeout: false,
            layout_strikeout: false,
            rotation: 0.0,
            advance_known: true,
            item_type: ItemType::Text,
            mcid: None,
            baseline_shift: 0.0,
        }
    }

    fn white_run(log: &PaintLog) -> ShownRun {
        log.shown_run(
            RunPaint {
                fill_color: Some(PAPER),
                stroke_color: Some([0, 0, 0]),
                render_mode: 0,
            },
            false,
        )
    }

    #[test]
    fn the_tolerance_is_inclusive_on_every_channel() {
        assert!(same_colour([255, 255, 255], [249, 249, 249]));
        assert!(!same_colour([255, 255, 255], [248, 255, 255]));
        assert!(same_colour([10, 20, 30], [16, 14, 36]));
    }

    #[test]
    fn rectangles_are_read_in_either_winding_and_with_a_closing_point() {
        let square = [(0.0, 0.0), (10.0, 0.0), (10.0, 5.0), (0.0, 5.0)];
        assert_eq!(rectangle(&square), Some(area(0.0, 0.0, 10.0, 5.0)));
        let mut reversed = square;
        reversed.reverse();
        assert_eq!(rectangle(&reversed), Some(area(0.0, 0.0, 10.0, 5.0)));
        let plumb_first = [(0.0, 0.0), (0.0, 5.0), (10.0, 5.0), (10.0, 0.0), (0.0, 0.0)];
        assert_eq!(rectangle(&plumb_first), Some(area(0.0, 0.0, 10.0, 5.0)));
        let skewed = [(0.0, 0.0), (10.0, 1.0), (10.0, 5.0), (0.0, 5.0)];
        assert_eq!(rectangle(&skewed), None);
        let flat = [(0.0, 0.0), (10.0, 0.0), (10.0, 0.0), (0.0, 0.0)];
        assert_eq!(rectangle(&flat), None);
    }

    #[test]
    fn overlapping_rectangles_in_one_fill_paint_their_colour_somewhere() {
        // Even-odd holes, or nonzero windings that cancel: the shape is not
        // read.
        let log = log_after("0 0 100 100 re 20 20 60 60 re f*", PAPER);
        assert_eq!(
            log.paints,
            vec![Paint::Scattered {
                area: area(0.0, 0.0, 100.0, 100.0),
                colour: PAPER,
            }]
        );
        let log = log_after("0 0 50 10 re 60 0 50 10 re f", PAPER);
        assert_eq!(log.paints.len(), 2);
        assert!(log
            .paints
            .iter()
            .all(|p| matches!(p, Paint::Solid { opaque: true, .. })));
    }

    #[test]
    fn curves_and_broken_paths_paint_their_colour_somewhere() {
        let log = log_after("0 0 m 10 0 l 10 10 5 15 0 10 c h f", PAPER);
        assert!(matches!(log.paints[..], [Paint::Scattered { .. }]));
        let log = log_after("10 0 l f", PAPER);
        assert!(matches!(
            log.paints[..],
            [Paint::Scattered {
                area: Area::EVERYWHERE,
                ..
            }]
        ));
    }

    #[test]
    fn a_stroke_paints_its_colour_as_wide_as_the_line() {
        let log = log_after("4 w 10 10 m 100 10 l S", PAPER);
        assert_eq!(
            log.paints,
            vec![Paint::Scattered {
                area: area(6.0, 6.0, 104.0, 14.0),
                colour: [0, 0, 0],
            }]
        );
        let log = log_after("q 30 w Q 0 0 100 100 re W n", PAPER);
        assert!(log.paints.is_empty(), "clip-only paths paint nothing");
    }

    #[test]
    fn white_text_over_a_thick_dark_stroke_is_kept() {
        let item = run("words", 100.0, 100.0, 50.0);
        let log = log_after("30 w 60 105 m 360 105 l S", PAPER);
        assert!(!log.hides(&item, &white_run(&log)));
        // A white stroke under white text changes nothing.
        let mut log = empty_log();
        for op in Content::decode(b"30 w 60 105 m 360 105 l S")
            .unwrap()
            .operations
        {
            let paint = RunPaint {
                fill_color: Some(PAPER),
                stroke_color: Some(PAPER),
                render_mode: 0,
            };
            log.observe(
                &op.operator,
                &op.operands,
                &IDENTITY,
                ClipBounds::None,
                paint,
            );
        }
        assert!(log.hides(&item, &white_run(&log)));
    }

    #[test]
    fn a_rectangular_clip_cuts_a_fill_and_an_unknown_clip_scatters_it() {
        let mut log = empty_log();
        for op in Content::decode(b"0 0 100 100 re f").unwrap().operations {
            log.observe(
                &op.operator,
                &op.operands,
                &IDENTITY,
                ClipBounds::Rect([50.0, 50.0, 200.0, 200.0]),
                fill_paint([0, 0, 0]),
            );
        }
        for op in Content::decode(b"0 0 100 100 re f").unwrap().operations {
            log.observe(
                &op.operator,
                &op.operands,
                &IDENTITY,
                ClipBounds::Unknown,
                fill_paint([0, 0, 0]),
            );
        }
        assert_eq!(
            log.paints,
            vec![
                Paint::Solid {
                    area: area(50.0, 50.0, 100.0, 100.0),
                    colour: [0, 0, 0],
                    opaque: true
                },
                Paint::Scattered {
                    area: area(0.0, 0.0, 100.0, 100.0),
                    colour: [0, 0, 0],
                }
            ]
        );
    }

    #[test]
    fn white_on_the_bare_page_and_on_white_is_hidden_but_not_on_dark() {
        let item = run("words", 100.0, 100.0, 50.0);
        let log = empty_log();
        assert!(log.hides(&item, &white_run(&log)));
        let log = log_after("0 90 300 30 re f", PAPER);
        assert!(log.hides(&item, &white_run(&log)));
        let log = log_after("0 90 300 30 re f", [40, 40, 40]);
        assert!(!log.hides(&item, &white_run(&log)));
    }

    #[test]
    fn a_fill_that_only_grazes_the_run_decides_nothing_alone() {
        // A dark rule under the descenders: the run's box overlaps it but is
        // not held by it, so the colour under the run is mixed.
        let item = run("words", 100.0, 100.0, 50.0);
        let log = log_after("0 96 300 2 re f", [0, 0, 0]);
        assert!(!log.hides(&item, &white_run(&log)));
    }

    #[test]
    fn a_later_fill_judges_nothing_about_colour_but_may_cover() {
        let item = run("words", 100.0, 100.0, 50.0);
        let mut log = empty_log();
        let shown = white_run(&log);
        for op in Content::decode(b"0 90 300 30 re f").unwrap().operations {
            log.observe(
                &op.operator,
                &op.operands,
                &IDENTITY,
                ClipBounds::None,
                fill_paint([0, 0, 0]),
            );
        }
        // Painted after the run: the run is covered, whatever its colour.
        assert!(log.hides(&item, &shown));
        let unjudged = ShownRun {
            seq: 0,
            ink: None,
            off: false,
        };
        assert!(log.hides(&item, &unjudged));
    }

    #[test]
    fn graphics_state_entries_read_as_compositing() {
        let doc = Document::with_version("1.5");
        let read = |state: Dictionary| StateUpdate::read(&doc, &state);
        assert_eq!(
            read(lopdf::dictionary! { "ca" => 1, "BM" => "Normal", "SMask" => "None" }),
            StateUpdate {
                full_alpha: Some(true),
                normal_blend: Some(true),
                no_soft_mask: Some(true),
                no_overprint: None,
            }
        );
        let translucent = read(lopdf::dictionary! { "ca" => 0.5, "OP" => true });
        assert_eq!(translucent.full_alpha, Some(false));
        assert_eq!(translucent.no_overprint, Some(false));
        let blended = read(lopdf::dictionary! { "BM" => vec![Object::Name(b"Multiply".to_vec())] });
        assert_eq!(blended.normal_blend, Some(false));
        let stroke_only = read(lopdf::dictionary! { "CA" => 0.2, "LW" => 3 });
        assert_eq!(stroke_only, StateUpdate::default());
    }

    #[test]
    fn an_unbalanced_q_in_a_form_leaves_the_compositing_unknown() {
        let doc = Document::with_version("1.5");
        let mut log = empty_log();
        log.observe("q", &[], &IDENTITY, ClipBounds::None, fill_paint(PAPER));
        log.enter_form(&doc, &Dictionary::new(), &IDENTITY);
        log.observe_in_form("Q", &[], &IDENTITY, fill_paint(PAPER));
        assert_eq!(log.compositing, Compositing::UNKNOWN);
        log.leave_form(true);
        assert_eq!(log.compositing, Compositing::default());
        assert_eq!(log.saved.len(), 1);
    }

    #[test]
    fn a_form_walk_cut_short_leaves_its_area_unknown() {
        let doc = Document::with_version("1.5");
        let form = lopdf::dictionary! { "BBox" => vec![0.into(), 0.into(), 100.into(), 50.into()] };
        let mut log = empty_log();
        log.enter_form(&doc, &form, &IDENTITY);
        log.leave_form(true);
        assert!(log.paints.is_empty());
        log.enter_form(&doc, &form, &IDENTITY);
        log.leave_form(false);
        assert!(matches!(log.paints[..], [Paint::Unknown { .. }]));
    }

    #[test]
    fn a_page_painting_past_the_cap_judges_nothing() {
        let item = run("words", 100.0, 100.0, 50.0);
        let mut log = empty_log();
        for _ in 0..=MAX_PAINTS {
            log.record(area(0.0, 0.0, 1.0, 1.0), Kind::Unknown, ClipBounds::None);
        }
        assert!(!log.hides(&item, &white_run(&log)));
    }
}
