use try_lib::gql;
use try_lib::hermes::{self, ChannelEntry, Settings};

fn main() {
    let app = tauri::Builder::default()
        .build(tauri::generate_context!())
        .expect("failed to build tauri app");
    let handle = app.handle().clone();
    std::thread::spawn(move || {
        let (tx, rx) = kanal::unbounded();
        hermes::spawn(handle.clone(), rx);
        let send = |channels: Vec<(&str, u64)>| {
            tx.send(Settings {
                channels: channels
                    .into_iter()
                    .map(|(login, id)| ChannelEntry {
                        login: login.into(),
                        id: Some(id),
                        display_name: None,
                    })
                    .collect(),
                notify_title_changes: false,
                sound: false,
            })
            .unwrap();
        };

        let users = gql::fetch_users(&[], &["shroud".into()]).unwrap();
        let shroud = users[0].channel_id;
        println!("--- phase 1: subscribe shroud({shroud}), run 8s");
        send(vec![("shroud", shroud)]);
        std::thread::sleep(std::time::Duration::from_secs(8));

        let users = gql::fetch_users(&[], &["lolitofdez".into()]).unwrap();
        let lolito = users[0].channel_id;
        println!("--- phase 2: ADD lolitofdez({lolito}) on the live session, run 8s");
        send(vec![("shroud", shroud), ("lolitofdez", lolito)]);
        std::thread::sleep(std::time::Duration::from_secs(8));

        println!("--- phase 3: REMOVE lolitofdez on the live session, run 8s");
        send(vec![("shroud", shroud)]);
        std::thread::sleep(std::time::Duration::from_secs(8));
        println!("--- done");
        handle.exit(0);
    });
    app.run(|_, _| {});
}
