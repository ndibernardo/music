use std::path::PathBuf;
use std::rc::Rc;

use gtk4::Application;
use gtk4::prelude::*;

use crate::library::db::Db;
use crate::library::track::Track;
use crate::player::PlaybackState;
use crate::player::PlayerHandle;

pub fn run(
    db: Rc<Db>,
    db_path: PathBuf,
    player: PlayerHandle,
    state_rx: async_channel::Receiver<PlaybackState>,
    queue_rx: async_channel::Receiver<Vec<Track>>,
) {
    let app = Application::builder()
        .application_id("io.github.ndibernardo.music")
        .build();

    app.connect_activate(move |app| {
        // The one stylesheet install — before any widget is built.
        crate::ui::style::install();
        crate::ui::main_window::build(
            app,
            Rc::clone(&db),
            db_path.clone(),
            player.clone(),
            state_rx.clone(),
            queue_rx.clone(),
        )
        .present();
    });

    app.run();
}
