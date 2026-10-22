use super::*;

#[test]
fn answer_has_the_parsed_shape() {
    let a = answer(Location {
        lat: 1.5,
        lon: -2.0,
    });
    let tokens: Vec<&str> = a.split([' ', '\n']).filter(|t| !t.is_empty()).collect();
    assert_eq!(tokens, ["+", "1.5", "-2", "0", "0", "0"]);
    assert!(a.len() < 0x100);
}

#[test]
fn default_is_not_the_unknown_value() {
    assert!(Location::default().check().is_ok());
    assert!(Location { lat: 0.0, lon: 0.0 }.check().is_err());
    assert!(
        Location {
            lat: -0.0,
            lon: 0.0
        }
        .check()
        .is_err()
    );
    assert!(Location { lat: 0.0, lon: 2.0 }.check().is_ok());
}
