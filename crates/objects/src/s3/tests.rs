use super::*;
mod http;

#[test]
fn sigv4_get_matches_independent_hmac_vector() {
    // Fixed SigV4 vector independently calculated using Python hashlib/hmac; public example credentials.
    let credentials = Credentials {
        access_key: "AKIAIOSFODNN7EXAMPLE".into(),
        secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
    };
    let mut headers = HeaderMap::new();
    for (k, v) in [
        ("host", "examplebucket.s3.amazonaws.com"),
        ("range", "bytes=0-9"),
        ("x-amz-date", "20130524T000000Z"),
        (
            "x-amz-content-sha256",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
    ] {
        headers.insert(k, v.parse().unwrap());
    }
    let signed = sign(
        "GET",
        "/test.txt",
        &headers,
        "20130524T000000Z",
        "us-east-1",
        &credentials,
    )
    .unwrap();
    assert!(
        signed.ends_with(
            "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        )
    );
}
