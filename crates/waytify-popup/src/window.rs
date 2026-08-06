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

pub fn build(app: &Application, options: Options) -> Result<()> {
    let window =
        ApplicationWindow::builder().application(app).default_width(WIDTH).resizable(false).build();

    // Layer shell must be initialised before the window is realised. Doing it
    // afterwards silently yields an ordinary toplevel with no anchoring applied.
    window.init_layer_shell();
    window.set_namespace(Some(NAMESPACE));

    // Overlay rather than Top, so the popup sits above a bar that is itself on
    // the top layer, which is where Waybar puts itself by default.
    window.set_layer(Layer::Overlay);

    // OnDemand lets the window take a click without stealing keyboard focus from
    // whatever the user was typing in. Exclusive would hijack the keyboard for a
    // window that has nothing to type into.
    window.set_keyboard_mode(KeyboardMode::OnDemand);

    window.set_anchor(Edge::Top, true);
    window.set_margin(Edge::Top, EDGE_MARGIN);
    place(&window, options.at);

    style::install(&window);

    let client = Rc::new(client::connect());
    let ui = Rc::new(ui::Ui::build(Rc::clone(&client)));
    window.set_child(Some(&ui.root));

    dismiss_on_escape(&window);
    ui::drive(Rc::clone(&ui), window.clone(), client);

    if options.show_on_start {
        window.present();
    }
    Ok(())
}

/// Apply a show, hide, or toggle from the daemon.
///
/// Hiding rather than destroying: this process stays resident so that reopening
/// is instant. Starting GTK costs over a tenth of a second before anything is
/// drawn, which is enough to feel broken on a button press.
pub fn apply(window: &ApplicationWindow, action: PopupAction) {
    match action {
        PopupAction::Hide => window.set_visible(false),
        PopupAction::Show { at } => {
            place(window, at);
            window.present();
        }
        PopupAction::Toggle { at } => {
            if window.is_visible() {
                window.set_visible(false);
            } else {
                place(window, at);
                window.present();
            }
        }
    }
}

/// Position the popup horizontally under wherever the click landed.
///
/// Waybar does not report where a module sits, so the click position is the only
/// signal for which monitor the user meant and roughly where to appear. With
/// nothing to go on, fall back to the top right, where most bars keep their tray.
fn place(window: &ApplicationWindow, at: Option<waytify_ipc::Point>) {
    let Some(point) = at else {
        window.set_anchor(Edge::Left, false);
        window.set_anchor(Edge::Right, true);
        window.set_margin(Edge::Right, EDGE_MARGIN);
        return;
    };

    // Anchoring left with a margin is how a layer surface says "this far from the
    // left edge of the output". Centring the window under the click reads as the
    // popup belonging to the thing that was clicked. The compositor keeps it on
    // screen, so a click near an edge does not push it off.
    window.set_anchor(Edge::Right, false);
    window.set_anchor(Edge::Left, true);
    window.set_margin(Edge::Left, (point.x - WIDTH / 2).max(EDGE_MARGIN));
}

/// Escape closes the window.
///
/// Click-outside dismissal needs a second surface to catch the click and is
/// handled separately; this is the keyboard half, and it works today because the
/// surface already takes keyboard input on demand.
fn dismiss_on_escape(window: &ApplicationWindow) {
    let keys = gtk4::EventControllerKey::new();
    let target = window.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk4::gdk::Key::Escape {
            target.set_visible(false);
            return gtk4::glib::Propagation::Stop;
        }
        gtk4::glib::Propagation::Proceed
    });
    window.add_controller(keys);
}
