//! The layer surface: placement, stacking, input behaviour, and show/hide.

use crate::{Options, client, style, ui};
use anyhow::Result;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::rc::Rc;
use waytify_ipc::PopupAction;

/// Wayland namespace for the surface. Compositor rules match on this, so it is
/// documented and stable.
const NAMESPACE: &str = "waytify";

/// Gap between the popup and the edge it hangs from, in logical pixels.
const EDGE_MARGIN: i32 = 8;

const WIDTH: i32 = 380;

/// The window, plus the invisible surface that catches clicks outside it.
pub struct Popup {
    window: ApplicationWindow,
    dismiss: ApplicationWindow,
}

impl Popup {
    /// Apply a show, hide, or toggle from the daemon.
    ///
    /// Hiding rather than destroying: this process stays resident so reopening is
    /// instant. Starting GTK costs over a tenth of a second before anything is
    /// drawn, which is enough to feel broken on a button press.
    pub fn apply(&self, action: PopupAction) {
        match action {
            PopupAction::Hide => self.hide(),
            PopupAction::Show { at } => self.show(at),
            PopupAction::Toggle { at } => {
                if self.window.is_visible() {
                    self.hide();
                } else {
                    self.show(at);
                }
            }
        }
    }

    fn show(&self, at: Option<waytify_ipc::Point>) {
        place(&self.window, at);

        // Same output as the popup, otherwise a click on the monitor the popup
        // opened on would not be caught.
        self.dismiss.set_monitor(self.window.monitor().as_ref());
        self.dismiss.present();
        self.window.present();
    }

    fn hide(&self) {
        self.window.set_visible(false);
        self.dismiss.set_visible(false);
    }

    /// Mark the displayed state as stale when the daemon goes away.
    pub fn set_offline(&self, offline: bool) {
        let Some(child) = self.window.child() else { return };
        if offline {
            child.add_css_class("offline");
        } else {
            child.remove_css_class("offline");
        }
    }
}

pub fn build(app: &Application, options: Options) -> Result<()> {
    let dismiss = build_dismiss_surface(app);

    let window =
        ApplicationWindow::builder().application(app).default_width(WIDTH).resizable(false).build();

    // Layer shell must be initialised before the window is realised. Doing it
    // afterwards silently yields an ordinary toplevel with no anchoring applied.
    // Named so the window node itself can be made transparent. Without that, the
    // theme's own window background paints a square behind the content and the
    // rounded corners sit on top of it instead of cutting through to the desktop.
    window.set_widget_name("waytify-window");

    window.init_layer_shell();
    window.set_namespace(Some(NAMESPACE));

    // Overlay, so the popup is above the bar it was opened from and above any
    // notification surface. Exclusive zones are still honoured from here, so it
    // lands below the bar rather than across it.
    window.set_layer(Layer::Overlay);

    // OnDemand lets the window take a click without stealing keyboard focus from
    // whatever the user was typing in. Exclusive would hijack the keyboard for a
    // window that has nothing to type into.
    window.set_keyboard_mode(KeyboardMode::OnDemand);

    // Zero means "keep clear of surfaces that reserved space", which is what puts
    // the popup below a bar rather than on top of it. It is the protocol default,
    // but set explicitly because the observed behaviour without it was not
    // consistent, and a window overlapping the bar it was opened from is the sort
    // of thing that should not depend on a default.
    window.set_exclusive_zone(0);

    window.set_anchor(Edge::Top, true);
    window.set_margin(Edge::Top, EDGE_MARGIN);
    place(&window, options.at);

    style::install(&window);

    let client = Rc::new(client::connect());
    let ui = Rc::new(ui::Ui::build(Rc::clone(&client)));
    window.set_child(Some(&ui.root));

    let popup = Rc::new(Popup { window: window.clone(), dismiss });
    dismiss_on_escape(&popup);
    catch_outside_clicks(&popup);
    ui::drive(Rc::clone(&ui), Rc::clone(&popup), client);

    if options.show_on_start {
        popup.show(options.at);
    }
    Ok(())
}

/// A transparent, full-screen surface that sits behind the popup.
///
/// Layer surfaces do not get a focus-out event, so there is no notification that
/// the user clicked elsewhere. The workable approach is to cover the screen with
/// something that can receive that click, on a lower layer than the popup so it
/// never intercepts the popup's own buttons.
fn build_dismiss_surface(app: &Application) -> ApplicationWindow {
    let surface = ApplicationWindow::builder().application(app).build();
    surface.init_layer_shell();
    surface.set_namespace(Some("waytify-dismiss"));

    // One layer below the popup, so the ordering between them is decided by the
    // protocol rather than by which was created first. Still above ordinary
    // windows, which is what it needs to catch a click on any of them.
    surface.set_layer(Layer::Top);
    // Never take the keyboard. This surface exists only to absorb one click.
    surface.set_keyboard_mode(KeyboardMode::None);

    // Keep clear of the bar. Covering it would mean the first click on any bar
    // module gets eaten by dismissal, including the waytify module itself, which
    // would make the widget feel unresponsive rather than dismissive.
    surface.set_exclusive_zone(0);

    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        surface.set_anchor(edge, true);
    }

    // The name goes on the window, not just the child. A GTK window paints its
    // own background from the theme before any child draws, so naming only the
    // child leaves an opaque sheet over the screen that happens to have a
    // transparent box on top of it.
    surface.set_widget_name("waytify-dismiss");

    // GtkApplicationWindow ships with the `background` style class, which paints
    // the theme's window colour. A transparent rule in our own stylesheet is not
    // enough on its own, so the class comes off as well.
    //
    // This matters far more than "an invisible window should be invisible". While
    // this surface was opaque, the popup above it did not render at all: mapped at
    // the right size, reported by the compositor, and never painted. Presumably a
    // full-screen opaque surface lets the compositor treat what is behind it as
    // occluded. Whatever the mechanism, an opaque catcher costs you the window it
    // was meant to serve, so keep it transparent.
    surface.remove_css_class("background");

    // An empty box rather than no child, so a stylesheet has something to select
    // if anyone wants a dimming backdrop rather than an invisible one.
    let backdrop = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    backdrop.add_css_class("waytify-backdrop");
    surface.set_child(Some(&backdrop));
    surface
}

fn catch_outside_clicks(popup: &Rc<Popup>) {
    let click = gtk4::GestureClick::new();
    let handler = Rc::clone(popup);
    click.connect_pressed(move |_, _, _, _| handler.hide());
    popup.dismiss.add_controller(click);
}

/// Position the popup horizontally under wherever the click landed.
///
/// Waybar does not report where a module sits, so the click position is the only
/// signal for which monitor the user meant and roughly where to appear. With
/// nothing to go on, fall back to the top right, where most bars keep their tray.
fn place(window: &ApplicationWindow, at: Option<waytify_ipc::Point>) {
    let Some(point) = at else {
        // Let the compositor choose the output, which means whichever has focus.
        window.set_monitor(None);
        window.set_anchor(Edge::Left, false);
        window.set_anchor(Edge::Right, true);
        window.set_margin(Edge::Right, EDGE_MARGIN);
        return;
    };

    // Bind the surface to the output the pointer is on. Without this the
    // compositor picks, and on a multi-monitor setup the window opens on the
    // wrong screen while the margin below is computed for the right one.
    let monitor = monitor_containing(window, point);
    window.set_monitor(monitor.as_ref());

    // Margins on a layer surface are measured from the output's own edge, so the
    // global pointer position has to be made relative to that output first.
    let origin_x = monitor.as_ref().map_or(0, |m| m.geometry().x());
    let available = monitor.as_ref().map_or(i32::MAX, |m| m.geometry().width());

    // Centre it under the click, then keep it fully on screen. Clamping here
    // rather than leaving it to the compositor means the window is where the
    // margin says it is, which matters for the pointer-relative maths.
    let centred = point.x - origin_x - WIDTH / 2;
    let furthest = (available - WIDTH - EDGE_MARGIN).max(EDGE_MARGIN);

    window.set_anchor(Edge::Right, false);
    window.set_anchor(Edge::Left, true);
    window.set_margin(Edge::Left, centred.clamp(EDGE_MARGIN, furthest));
}

/// The output containing a global point.
fn monitor_containing(
    window: &ApplicationWindow,
    point: waytify_ipc::Point,
) -> Option<gtk4::gdk::Monitor> {
    let display = WidgetExt::display(window);
    display
        .monitors()
        .into_iter()
        .flatten()
        .filter_map(|object| object.downcast::<gtk4::gdk::Monitor>().ok())
        .find(|monitor| {
            let g = monitor.geometry();
            (g.x()..g.x() + g.width()).contains(&point.x)
                && (g.y()..g.y() + g.height()).contains(&point.y)
        })
}

/// Escape closes the window. Works because the surface takes keyboard input on
/// demand, so it has focus while the pointer is over it.
fn dismiss_on_escape(popup: &Rc<Popup>) {
    let keys = gtk4::EventControllerKey::new();
    let target = Rc::clone(popup);
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk4::gdk::Key::Escape {
            target.hide();
            return gtk4::glib::Propagation::Stop;
        }
        gtk4::glib::Propagation::Proceed
    });
    popup.window.add_controller(keys);
}
