use super::{archive, archive_role, config, served_module};

#[test]
fn archive_roles_follow_entry_names() {
    let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(
        archive_role(&names(&[
            "build/main/bin/dc1/net.bin",
            "build/main/bin/dc1/net-patch18.bin",
            "build/main/text1/2.networking"
        ]))
        .starts_with("fits patch_archive")
    );
    assert!(archive_role(&names(&["2.common", "2.networking"])).starts_with("fits text_archive"));
    assert!(archive_role(&names(&["dc1/net.bin", "dc1/ai.bin"])).starts_with("fits neither"));
    // An older patch archive also carries text tables, but under full paths.
    assert!(
        archive_role(&names(&[
            "build/main/bin/dc1/net.bin",
            "build/main/text1/2.networking"
        ]))
        .starts_with("fits neither: a patch archive")
    );
    assert!(archive_role(&names(&["build/main/text1/2.networking"])).starts_with("fits neither"));
}

/// Older archives also hold a config module; only the latest update's archive is accepted.
#[test]
fn only_the_latest_update_s_patch_archive_is_served() {
    let cfg: config::Config = toml::from_str(
        "public_host = \"192.168.1.50\"\nexecutable = \"x\"\npatch_archive = \"y\"\n\
         [http]\nbind = \"0.0.0.0:80\"\n",
    )
    .unwrap();
    let older = archive::build_archive(&[("build/main/bin/dc1/net.bin", b"module")]);
    let err = served_module(&cfg, &older).unwrap_err().to_string();
    assert!(err.contains("not the latest update's"), "{err}");
    // The update's archive passes this check (the dummy module then fails to parse).
    let latest = archive::build_archive(&[
        ("build/main/bin/dc1/net.bin", b"module"),
        ("build/main/bin/dc1/net-patch18.bin", b"module"),
    ]);
    let err = format!("{:#}", served_module(&cfg, &latest).unwrap_err());
    assert!(!err.contains("not the latest update's"), "{err}");
}
