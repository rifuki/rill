//! TEMPORARY probe: can a plain object read expose the wallet's rule set?
use sui_rpc::client::Client;
use sui_rpc::proto::sui::rpc::v2::GetObjectRequest;

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";
const WALLET: &str = "0x20391fa91aec7a12b6657902af80036e125d1beff6621fe2eb73cfd032a04e5d";

fn mask(paths: &[&str]) -> prost_types::FieldMask {
    prost_types::FieldMask { paths: paths.iter().map(|p| (*p).to_owned()).collect() }
}

#[tokio::test]
#[ignore]
async fn object_json_probe() {
    let client = Client::new(TESTNET).unwrap();

    for paths in [
        vec!["object_id", "version", "digest", "object_type", "owner", "json"],
        vec!["object_id", "contents"],
    ] {
        let mut request = GetObjectRequest::default();
        request.object_id = Some(WALLET.to_owned());
        request.read_mask = Some(mask(&paths));
        let resp = client
            .clone()
            .ledger_client()
            .get_object(request)
            .await;
        match resp {
            Err(status) => println!("mask {paths:?} -> ERROR {}: {}", status.code(), status.message()),
            Ok(r) => {
                let o = r.into_inner().object.unwrap_or_default();
                println!("--- mask {paths:?} ---");
                println!("json present  : {}", o.json.is_some());
                if let Some(v) = &o.json {
                    let s = format!("{v:?}");
                    println!("json (raw dbg): {}", &s[..s.len().min(4000)]);
                }
                println!("contents      : {} bytes", o.contents.as_ref().and_then(|c| c.value.as_ref()).map(|b| b.len()).unwrap_or(0));
                if let Some(b) = o.contents.as_ref().and_then(|c| c.value.as_ref()) {
                    println!("contents hex  : {}", b.iter().map(|x| format!("{x:02x}")).collect::<String>());
                }
            }
        }
    }
}
