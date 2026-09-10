I have a server and a client.
The client establishes a connection on the server. over a quic http3 connection over scion. There is already an example api.rs and you should use this as a template. In an interval of 5s pull the data from the client. Use the connect http call to get a bidirectional channel.

The protocol to communicate over the channel looks like this:

```json
{
    "type": "data",
    "id": 42,
    "payload": {
        "index": 0 // 0 means from beginning of time, n means from n index until now
    } // Payload on command empty
}
```
Response from the client looks like this:
```json
{
    "type": "data",
    "id": 42, // Same ID as request
    "payload": {
        "data": [ // Batched data
            {
                "timestamp": "standard formatted unixtimestamp",
                "value1": "Any data",
                "value2": "Any data",
                "index": 1 // Increasing index after each data request
            },
            {
                "timestamp": "standard formatted unixtimestamp",
                "value1": "Any data",
                "value2": "Any data",
                "index": 2 // Increasing index after each data request
            },
        ]
    }
}
```

For now print the data on stdout and write it into a local file named: `data.json`

Don't mess with the network layer just code the http api and the client to test it it.

This is an example client implementation:

```
        let authority = format!("{gateway_domain}:{port}");
        let req = http::Request::builder()
            .method(http::Method::CONNECT)
            .uri(format!("https://{authority}"))
            .body(())
            .context("failed to build CONNECT request")?;

        let (response, writer) = self
            .client // scion_quic::h3::client::Http3Client
            .request_with_writer(req)
            .await
            .with_context(|| {
                format!("Failed to request CONNECT stream for authority {authority}")
            })?;

        // Create a streaming request to the gateway domain.
        let response = response.await.with_context(|| {
            format!(
                "Failed to receive CONNECT response headers for authority {authority}"
            )
        })?;

        if !response.status().is_success() {
            anyhow::bail!(
                "CONNECT request to authority {authority} failed with status {}",
                response.status()
            );
        }

        // Create a duplex stream for the CONNECT tunnel.
        Ok(scion_quic::h3::client::H3DuplexStream::new(writer, response.into_body()))
```