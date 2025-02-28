use crate::builders::{CrateBuilder, PublishBuilder};
use crate::util::{RequestHelper, TestApp};
use crates_io::views::GoodCrate;
use crates_io_tarball::TarballBuilder;
use flate2::Compression;
use http::StatusCode;
use std::io;
use std::io::Read;

#[test]
fn tarball_between_default_axum_limit_and_max_upload_size() {
    let max_upload_size = 5 * 1024 * 1024;
    let (app, _, _, token) = TestApp::full()
        .with_config(|config| {
            config.max_upload_size = max_upload_size;
            config.max_unpack_size = max_upload_size;
        })
        .with_token();

    let tarball = {
        let mut builder = TarballBuilder::new();

        let data = b"[package]\nname = \"foo\"\nversion = \"1.1.0\"\ndescription = \"description\"\nlicense = \"MIT\"\n" as &[_];

        let mut header = tar::Header::new_gnu();
        assert_ok!(header.set_path("foo-1.1.0/Cargo.toml"));
        header.set_size(data.len() as u64);
        header.set_cksum();
        assert_ok!(builder.as_mut().append(&header, data));

        // `data` is smaller than `max_upload_size`, but bigger than the regular request body limit
        let data = &[b'a'; 3 * 1024 * 1024] as &[_];

        let mut header = tar::Header::new_gnu();
        assert_ok!(header.set_path("foo-1.1.0/big-file.txt"));
        header.set_size(data.len() as u64);
        header.set_cksum();
        assert_ok!(builder.as_mut().append(&header, data));

        // We explicitly disable compression to be able to influence the final tarball size
        builder.build_with_compression(Compression::none())
    };

    let (json, _tarball) = PublishBuilder::new("foo", "1.1.0").build();
    let body = PublishBuilder::create_publish_body(&json, &tarball);

    let response = token.put("/api/v1/crates/new", body);
    assert_eq!(response.status(), StatusCode::OK);
    let json: GoodCrate = response.good();
    assert_eq!(json.krate.name, "foo");
    assert_eq!(json.krate.max_version, "1.1.0");

    app.run_pending_background_jobs();
    assert_eq!(app.stored_files().len(), 2);
}

#[test]
fn tarball_bigger_than_max_upload_size() {
    let max_upload_size = 5 * 1024 * 1024;
    let (app, _, _, token) = TestApp::full()
        .with_config(|config| {
            config.max_upload_size = max_upload_size;
            config.max_unpack_size = max_upload_size;
        })
        .with_token();

    let tarball = {
        // `data` is bigger than `max_upload_size`
        let data = &[b'a'; 6 * 1024 * 1024] as &[_];

        let mut builder = TarballBuilder::new();

        let mut header = tar::Header::new_gnu();
        assert_ok!(header.set_path("foo-1.1.0/Cargo.toml"));
        header.set_size(data.len() as u64);
        header.set_cksum();
        assert_ok!(builder.as_mut().append(&header, data));

        // We explicitly disable compression to be able to influence the final tarball size
        builder.build_with_compression(Compression::none())
    };

    let (json, _tarball) = PublishBuilder::new("foo", "1.1.0").build();
    let body = PublishBuilder::create_publish_body(&json, &tarball);

    let response = token.put::<()>("/api/v1/crates/new", body);
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.into_json(),
        json!({ "errors": [{ "detail": format!("max upload size is: {max_upload_size}") }] })
    );

    app.run_pending_background_jobs();
    assert!(app.stored_files().is_empty());
}

#[test]
fn new_krate_gzip_bomb() {
    let (app, _, _, token) = TestApp::full()
        .with_config(|config| {
            config.max_upload_size = 3000;
            config.max_unpack_size = 2000;
        })
        .with_token();

    let len = 512 * 1024;
    let mut body = Vec::new();
    io::repeat(0).take(len).read_to_end(&mut body).unwrap();

    let crate_to_publish = PublishBuilder::new("foo", "1.1.0").add_file("foo-1.1.0/a", body);

    let response = token.publish_crate(crate_to_publish);
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.into_json(),
        json!({ "errors": [{ "detail": "uploaded tarball is malformed or too large when decompressed" }] })
    );

    assert!(app.stored_files().is_empty());
}

#[test]
fn new_krate_too_big() {
    let (app, _, user) = TestApp::full()
        .with_config(|config| {
            config.max_upload_size = 3000;
            config.max_unpack_size = 2000;
        })
        .with_user();

    let builder = PublishBuilder::new("foo_big", "1.0.0")
        .add_file("foo_big-1.0.0/big", &[b'a'; 2000] as &[_]);

    let response = user.publish_crate(builder);
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.into_json(),
        json!({ "errors": [{ "detail": "uploaded tarball is malformed or too large when decompressed" }] })
    );

    assert!(app.stored_files().is_empty());
}

#[test]
fn new_krate_too_big_but_whitelisted() {
    let (app, _, user, token) = TestApp::full().with_token();

    app.db(|conn| {
        CrateBuilder::new("foo_whitelist", user.as_model().id)
            .max_upload_size(2_000_000)
            .expect_build(conn);
    });

    let crate_to_publish = PublishBuilder::new("foo_whitelist", "1.1.0")
        .add_file("foo_whitelist-1.1.0/big", &[b'a'; 2000] as &[_]);

    token.publish_crate(crate_to_publish).good();

    let expected_files = vec![
        "crates/foo_whitelist/foo_whitelist-1.1.0.crate",
        "index/fo/o_/foo_whitelist",
    ];
    assert_eq!(app.stored_files(), expected_files);
}
