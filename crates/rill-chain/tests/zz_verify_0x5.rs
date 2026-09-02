//! Throwaway adversarial probe: read 0x5 owner/type over gRPC on both networks.
use sui_rpc::client::Client;
use sui_rpc::proto::sui::rpc::v2::GetObjectRequest;

async fn probe(net: &str, id: &str) {
    let endpoint = format!("https://fullnode.{net}.sui.io:443");
    let client = Client::new(&endpoint).unwrap();
    let mut request = GetObjectRequest::default();
    request.object_id = Some(id.to_owned());
    request.read_mask = Some(prost_types::FieldMask {
        paths: vec![
            "object_id".into(),
            "version".into(),
            "object_type".into(),
            "owner".into(),
        ],
    });
    println!("\n===== {net} {id} =====");
    match client.clone().ledger_client().get_object(request).await {
        Err(s) => println!("ERR code={:?} msg={}", s.code(), s.message()),
        Ok(r) => {
            let o = r.into_inner().object.unwrap();
            println!("object_id   = {:?}", o.object_id);
            println!("version     = {:?}", o.version);
            println!("object_type = {:?}", o.object_type);
            println!("owner       = {:?}", o.owner);
        }
    }
}

#[tokio::test]
#[ignore]
async fn object_5_on_both() {
    for net in ["mainnet", "testnet"] {
        probe(
            net,
            "0x0000000000000000000000000000000000000000000000000000000000000005",
        )
        .await;
    }
}
