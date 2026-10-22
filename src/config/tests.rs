use super::*;

#[test]
fn example_config_parses() {
    let text = include_str!("../../uc3-server.example.toml").replace("CHANGE-ME", "192.168.1.50");
    let cfg: Config = toml::from_str(&text).unwrap();
    assert_eq!(cfg.bind_ip, any_ip());
    assert_eq!((cfg.location.lat, cfg.location.lon), (1.0, 1.0));
    assert!(cfg.text_archive.is_none() && cfg.data_dir.is_none());
    assert!(cfg.playlists.min_players.is_empty());
}

#[test]
fn playlist_minimums_parse() {
    let text = include_str!("../../uc3-server.example.toml").replace("CHANGE-ME", "192.168.1.50")
        + "\n[playlists]\nmin_players = { \"*\" = 2, coop = 3 }\n";
    let cfg: Config = toml::from_str(&text).unwrap();
    let mins: Vec<_> = cfg
        .playlists
        .min_players
        .iter()
        .map(|(k, v)| (k.as_str(), *v))
        .collect();
    assert_eq!(mins, [("*", 2), ("coop", 3)]);
}

/// A data_dir that holds a game file is refused; one beside the game folder is fine.
#[test]
fn data_dir_must_not_hold_the_game_files() {
    let dir = std::env::temp_dir().join(format!("uc3-config-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("game")).unwrap();
    std::fs::create_dir_all(dir.join("static")).unwrap();
    std::fs::write(dir.join("game/exe"), b"x").unwrap();
    std::fs::write(dir.join("game/patch"), b"x").unwrap();
    let load = |data_dir: &str| {
        let text = format!(
            "public_host = \"192.168.1.50\"\nexecutable = \"game/exe\"\npatch_archive = \"game/patch\"\n\
             data_dir = \"{data_dir}\"\n[http]\nbind = \"0.0.0.0:80\"\n"
        );
        std::fs::write(dir.join("uc3-server.toml"), text).unwrap();
        Config::load(&dir.join("uc3-server.toml"))
    };
    let (root, game, separate) = (load("."), load("game"), load("static"));
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        root.is_err() && game.is_err(),
        "a folder holding the game files is refused"
    );
    assert!(separate.is_ok(), "{:?}", separate.err());
}

#[test]
fn public_host_must_be_reachable() {
    assert!(check_public_host("192.168.1.50").is_ok());
    assert!(check_public_host("server.example.lan").is_ok());
    for bad in ["", "0.0.0.0", "::", "CHANGE-ME", "a b"] {
        assert!(check_public_host(bad).is_err(), "{bad:?} must be rejected");
    }
}
