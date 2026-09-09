//! Asking Windows Terminal, through UI Automation, what a tab is displaying and
//! what its session is *actually* called.
//!
//! **The one fact nothing else exposes.** A tab renamed by the user keeps that
//! name forever and ignores every later `SetConsoleTitleW`; the console object
//! still holds what we wrote and `GetConsoleTitleW` still reads it back, so the
//! two halves of the console API agree with each other and neither can see the
//! tab. What UIA adds is the other side of the comparison —
//! `TermControlAutomationPeer::GetHelpTextCore()` returns `ControlCore::Title()`,
//! the pane's real console title, which a rename does not touch.
//!
//! Measured live on 2026-09-03, the whole basis of the detector:
//!
//! ```text
//! shown = ttt        real = ✋ what-is-next [78%]
//! ```
//!
//! A blocked session waiting for its user, behind a tab saying `ttt`.
//!
//! **Both strings come from this one pass, and that is deliberate.** The obvious
//! source for the displayed name is the window caption, which Windows Terminal
//! publishes as its active tab's title and which this process already reads for
//! attention. Two reasons not to use it here. It is only the tab's name under a
//! default setting: `showTerminalTitleInTitlebar: false` pins every caption to
//! the literal string `Windows Terminal` while each tab keeps its own title, so a
//! caption-based comparison would report every session on that machine as stale
//! at once. And reading the caption in one API and the pane in another puts the
//! two sides of the comparison at different instants, separated by a
//! cross-process round trip — long enough for a status change to land between
//! them and manufacture a disagreement. Reading the selected `TabItem`'s own
//! `Name` beside its pane closes both.
//!
//! **Only the selected tab answers, and that is structural.** XAML's `TabView`
//! realizes one `ContentPresenter`, so the `TermControl` search returns the panes
//! of the tab in front and of no other — measured 2/1/1/0/3 against windows of
//! six tabs. It is also the right limit: a stale glyph misleads precisely while
//! its tab is on screen.
//!
//! That is also what lets the pane search stay window-wide rather than scoped to
//! the selected tab's subtree. If a background tab stayed realized, `shown` and
//! the panes would come from different tabs and this would accuse a healthy one,
//! so it was measured directly on 2026-09-04: one window, six tabs, `shown` the
//! selected `🔵 ai-dashboard`, and exactly one realized pane reading that same
//! string. A realization lagging a tab switch could not report either way, since
//! a report needs the same pair twice across the grace.
//!
//! **A tab holding no `TermControl` is not a gap.** It reads as zero panes, which
//! `super::read_front` abstains on by name and logs; and a tab with no terminal
//! control in it — Windows Terminal's settings tab is the case — holds no
//! session, so there is no row the check could have been about.
//!
//! **A rename raises no event either, not even the caption one.** Measured
//! 2026-09-08 with a pid-scoped `EVENT_OBJECT_NAMECHANGE` hook and a control in
//! the same run: typing a new name raised one (caption `✋ bga-assistant` ->
//! `zzz`) and committing an empty title to reset it raised one, but a
//! double-click that *accepts the pre-filled name* raised nothing at all. Three
//! `OBJID_CURSOR` events bracket the gesture, so the rename box demonstrably
//! opened and closed; Windows Terminal simply does not publish a set whose value
//! is unchanged. That is the accidental rename, and it is the dangerous case: the
//! tab keeps showing a plausible but frozen status. So it is invisible at the
//! instant it happens by event *and* by delta, the committed string being
//! byte-identical, and only becomes observable when the row's status next moves
//! and the displayed name fails to follow. This is why the two-sample rule in
//! `super::stale_check` cannot be replaced by comparing consecutive readings, and
//! why the check is driven from the edges the app already has.
//!
//! **Polling, not events.** UIA raises no `Name` property-changed event for these
//! elements — measured over 15 real title changes with handlers at
//! `TreeScope_Subtree`, at desktop scope, and pinned per element, across
//! `LiveRegionChanged`, `Notification`, `Changes`, `TextEdit`, `StructureChanged`,
//! `ItemStatus` and `FullDescription`. So the caller reads this on an edge it
//! already has rather than on a timer.
//!
//! **What a pass costs: 95 ms cold, ~5 ms warm.** Measured 2026-09-04 on this
//! machine. Cold `CoInitializeEx` plus the first `CoCreateInstance` dominate; the
//! design notes elsewhere quote only the warm figure, which under-states the
//! first call by 20x. It is edge-triggered and off the attach lock, so the cold
//! cost is paid once per process and nothing in the steady state pays it — but do
//! not read this as a sub-10 ms operation when deciding where to call it from.
//!
//! **A costed option, deliberately not built.** On a caption event this module
//! could read `(shown, real)` and compare against the previous pair for that
//! surface, which separates the three causes of a caption change: `shown` moved
//! while `real` held still is a rename; both moved to another session's values is
//! a tab switch; both moved together to one string is our own write to a
//! following tab; `real` moved while `shown` held still is our own write to a
//! pinned tab. That is a positive discriminator over two read facts, and it would
//! settle a *deliberate* rename on the first pass instead of the second.
//!
//! It is not built because it buys least where it is needed most. The accidental
//! rename commits the name the box was pre-filled with, so `shown` is
//! byte-identical afterwards and there is no delta at all — measured above. It
//! also cannot remove the grace: "real moved, shown did not" is equally the
//! signature of our own write still propagating. So the gain is roughly 4.5s to
//! 2s on the badge for the visible case only, against another per-surface delta
//! map in the bookkeeping that has produced nearly every defect this feature has
//! had. Worth revisiting only as a *simplification* — if it can replace the
//! two-sample rule rather than sit beside it.
//!
//! **Never call this while `terminal_title` holds its console-attach lock** (a
//! private static, so there is nothing here to link to). It is a
//! cross-process call into a UI thread, measured with multi-hundred-millisecond
//! excursions on a busy terminal, and that lock serialises every title write.

use windows::core::{Interface, BSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, IUIAutomationSelectionPattern, TreeScope_Descendants, UIA_PATTERN_ID, UIA_PROPERTY_ID};

/// `UIA_ClassNamePropertyId` — what both searches match on.
const PROP_CLASS_NAME: UIA_PROPERTY_ID = UIA_PROPERTY_ID(30012);
/// `UIA_HelpTextPropertyId` — where `ControlCore::Title()` surfaces.
const PROP_HELP_TEXT: UIA_PROPERTY_ID = UIA_PROPERTY_ID(30013);
/// `UIA_SelectionPatternId`, on the tab strip.
const PATTERN_SELECTION: UIA_PATTERN_ID = UIA_PATTERN_ID(10001);
/// `UIA_AutomationIdPropertyId` — how the tab strip is found.
const PROP_AUTOMATION_ID: UIA_PROPERTY_ID = UIA_PROPERTY_ID(30011);
/// The XAML class of one terminal pane.
const TERM_CONTROL: &str = "TermControl";
/// The tab strip's `AutomationId`, which is what identifies it.
///
/// **Not its class.** Measured on the live tree 2026-09-04, the strip is
/// `ClassName = "ListView"`, `ControlType = List`, `AutomationId = "TabListView"`
/// — there is no element called `TabView` at all, so matching on that class found
/// nothing and this whole read returned `None` for every window, which would have
/// meant a detector that never fires. An `AutomationId` is also the right handle
/// on principle: it is the name the application chose for the element, where a
/// XAML class name is an implementation detail of the control it happens to be
/// built from.
///
/// Asking the strip which item is selected beats reading `IsSelected` per tab,
/// which was measured answering `NotSupported` for the selected tab unless
/// `ignoreDefaultValue` is false — a trap that reads as a blank rather than as an
/// error.
const TAB_STRIP_ID: &str = "TabListView";

/// What one terminal window is showing: the name on the tab in front, and the
/// real console title of each pane behind it.
pub struct Surface {
    /// The name on the tab in front. The untrusted half: a user may have
    /// overwritten it, and it is only ever the object of a comparison, never the
    /// string anything is named from.
    pub shown: String,
    /// One entry per *realized pane*, in tree order. `None` for a pane whose
    /// title could not be read.
    ///
    /// The count is the split answer, so a pane must never be silently dropped:
    /// discarding an unreadable one would turn a split surface into a
    /// single-pane reading and let the rule judge — and accuse — a tab it should
    /// have abstained on. An empty title is a real reading and is kept as
    /// `Some("")`; this dashboard writes those itself when it blanks a departed
    /// row.
    pub panes: Vec<Option<String>>,
}

/// Join this thread to the multi-threaded apartment, once.
///
/// MTA rather than STA deliberately. Both were measured working, and an STA
/// client needed no message pumping either — but an STA carries the *obligation*
/// to pump, and this runs on a worker thread with no message loop and no windows.
/// This process's own UI thread is a `MAINSTA`, and a UIA client is documented to
/// belong on a non-UI thread owning no windows, so never call this from there.
pub fn init_apartment() {
    // Already-initialised (`RPC_E_CHANGED_MODE`, `S_FALSE`) is expected and
    // harmless — the apartment is per-thread and something upstream may have
    // joined it. The reads themselves report any real failure.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

/// Read terminal window `hwnd`, or `None` when it could not be read at all.
///
/// A `None` covers an elevated Windows Terminal — a medium-integrity client cannot
/// reach an elevated process's UI, the same way the WinEvent hook already
/// receives nothing from one — and any COM failure. It is a failure to look, and
/// the caller must never read it as a finding.
pub fn read_surface(hwnd: isize) -> Option<Surface> {
    unsafe {
        let automation: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
        let window = automation.ElementFromHandle(HWND(hwnd as *mut _)).ok()?;

        // The displayed name, from the strip's own selection rather than from the
        // window caption. `FindFirst` answers `Err` when nothing matches
        // (windows-rs maps the NULL out-parameter to `E_POINTER`), so `.ok()?` is
        // both the lookup and the null check.
        let strip_cond = automation.CreatePropertyCondition(PROP_AUTOMATION_ID, &TAB_STRIP_ID.into()).ok()?;
        let strip = window.FindFirst(TreeScope_Descendants, &strip_cond).ok()?;
        let selection: IUIAutomationSelectionPattern = strip.GetCurrentPattern(PATTERN_SELECTION).ok()?.cast().ok()?;
        let selected = selection.GetCurrentSelection().ok()?;
        // Exactly one tab is in front. Anything else is a reading we cannot use.
        if selected.Length().ok()? != 1 {
            return None;
        }
        let shown = selected.GetElement(0).ok()?.CurrentName().ok()?.to_string();

        let pane_cond = automation.CreatePropertyCondition(PROP_CLASS_NAME, &TERM_CONTROL.into()).ok()?;
        let found = window.FindAll(TreeScope_Descendants, &pane_cond).ok()?;
        let count = found.Length().ok()?;
        let mut panes = Vec::with_capacity(count as usize);
        for i in 0..count {
            // One entry per pane whatever happens, so the count survives.
            //
            // `GetCurrentPropertyValueEx(.., TRUE)`, not `GetCurrentPropertyValue`.
            // The plain call is `..Ex(id, FALSE)`, which substitutes the
            // property's *default* value for an unsupported property and so hands
            // back an ordinary empty string; only `ignoreDefaultValue = TRUE`
            // yields `UiaGetReservedNotSupportedValue`, a `VT_UNKNOWN` that
            // `BSTR::try_from` refuses. Without it every unreadable pane read as a
            // pane titled "", which is a *reading* by this type's contract, so the
            // `None` slot could not occur and `read_front`'s
            // `session_title_unreadable` arm guarded a state that could not
            // happen. (This module had already measured the `ignoreDefaultValue`
            // behaviour for `IsSelected`, below, and the lesson did not carry the
            // two lines here.)
            panes.push(
                found
                    .GetElement(i)
                    .ok()
                    .and_then(|el| el.GetCurrentPropertyValueEx(PROP_HELP_TEXT, true).ok())
                    .and_then(|v| BSTR::try_from(&v).ok())
                    .map(|b| b.to_string()),
            );
        }
        Some(Surface { shown, panes })
    }
}
