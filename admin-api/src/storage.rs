//! S3 (Linode Object Storage) header-based SigV4 helpers. rusty-s3
//! presigned URLs fail against Linode's E1 cluster, so the signing is
//! done by hand.

#![allow(dead_code)]

use anyhow::{anyhow, Context};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use spin_sdk::http::{Method, Request, Response};
use url::Url;

type HmacSha256 = Hmac<Sha256>;

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

pub struct S3Creds {
    pub endpoint: String,
    pub host: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
}

pub fn load_s3_creds() -> anyhow::Result<S3Creds> {
    let endpoint = spin_sdk::variables::get("s3_endpoint").context("s3_endpoint not set")?;
    let region = spin_sdk::variables::get("s3_region").context("s3_region not set")?;
    let bucket = spin_sdk::variables::get("s3_bucket").context("s3_bucket not set")?;
    let access_key = spin_sdk::variables::get("s3_access_key").context("s3_access_key not set")?;
    let secret_key = spin_sdk::variables::get("s3_secret_key").context("s3_secret_key not set")?;

    if endpoint.is_empty() || bucket.is_empty() {
        return Err(anyhow!("S3 config incomplete — run `make provision` first"));
    }

    let parsed = Url::parse(&endpoint).context("s3_endpoint is not a valid URL")?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("no host in s3_endpoint"))?
        .to_string();

    Ok(S3Creds {
        endpoint: endpoint.trim_end_matches('/').to_string(),
        host,
        region,
        bucket,
        access_key,
        secret_key,
    })
}

fn sign_request(
    creds: &S3Creds,
    method: &str,
    canonical_uri: &str,
    payload_hash: &str,
) -> (String, String, String) {
    let now = chrono::Utc::now();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = now.format("%Y%m%d").to_string();

    let canonical_headers = format!(
        "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
        creds.host, payload_hash, amz_date
    );
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";

    let canonical_request = format!(
        "{}\n{}\n\n{}\n{}\n{}",
        method, canonical_uri, canonical_headers, signed_headers, payload_hash
    );

    let credential_scope = format!("{}/{}/s3/aws4_request", date_stamp, creds.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        amz_date,
        credential_scope,
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac_sha256(
        format!("AWS4{}", creds.secret_key).as_bytes(),
        date_stamp.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, creds.region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        creds.access_key, credential_scope, signed_headers, signature
    );

    let full_url = format!("{}{}", creds.endpoint, canonical_uri);
    (full_url, amz_date, authorization)
}

pub async fn put_object(key: &str, content_type: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
    let creds = load_s3_creds()?;
    let body_len = bytes.len();
    let payload_hash = sha256_hex(&bytes);
    let canonical_uri = format!("/{}/{}", creds.bucket, key);
    let (full_url, amz_date, authorization) =
        sign_request(&creds, "PUT", &canonical_uri, &payload_hash);

    let req = Request::builder()
        .method(Method::Put)
        .uri(full_url)
        .header("content-type", content_type)
        .header("content-length", body_len.to_string())
        .header("x-amz-content-sha256", payload_hash)
        .header("x-amz-date", amz_date)
        .header("authorization", authorization)
        .body(bytes)
        .build();

    let resp: Response = spin_sdk::http::send(req).await.map_err(|e| anyhow!("s3 put: {e}"))?;
    let status = *resp.status();
    if !(200..=299).contains(&status) {
        let body = String::from_utf8_lossy(resp.body()).to_string();
        return Err(anyhow!("S3 PUT {key} returned {status}: {body}"));
    }
    Ok(())
}

pub async fn get_object(key: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let creds = load_s3_creds()?;
    let canonical_uri = format!("/{}/{}", creds.bucket, key);
    let (full_url, amz_date, authorization) =
        sign_request(&creds, "GET", &canonical_uri, EMPTY_SHA256);

    let req = Request::builder()
        .method(Method::Get)
        .uri(full_url)
        .header("x-amz-content-sha256", EMPTY_SHA256)
        .header("x-amz-date", amz_date)
        .header("authorization", authorization)
        .body(Vec::new())
        .build();

    let resp: Response = spin_sdk::http::send(req).await.map_err(|e| anyhow!("s3 get: {e}"))?;
    let status = *resp.status();
    if status == 404 {
        return Ok(None);
    }
    if !(200..=299).contains(&status) {
        let body = String::from_utf8_lossy(resp.body()).to_string();
        return Err(anyhow!("S3 GET {key} returned {status}: {body}"));
    }
    Ok(Some(resp.body().to_vec()))
}
