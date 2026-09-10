# Ad hoc Hashtree connections

Hashtree can use FIPS paths across intermediate peers for blobs and Nostr
events. A transit peer only needs to route FIPS packets; it does not need a
matching Nostr subscription or membership in the endpoint application's data.

Use `server.fips_peers` for configured blob peers and physical connection
hints. Use `nostr.fips_pubsub_peers` for identities known to run the Nostr pubsub
service, including peers reachable through another router. These lists describe
different capabilities. A social follow or a transport connection alone does
not establish pubsub support.

For Nostr root events over an existing FIPS mesh, replace the placeholder with
the remote service's actual `npub`:

```toml
[nostr]
event_transport = "fips-local-only"
relays = []
fips_pubsub_peers = ["<service-peer-npub>"]
fips_trusted_raters = []
```

Configure each endpoint with the other's service identity. The pubsub adapter
validates identities and bounds the roster using its existing peer capacity.
An empty list retains the adapter's existing behavior with directly connected peers.
Reloading the daemon applies roster changes.

The daemon uses local connection observations to guide peer selection. To also
use signed machine ratings from identities you trust, add their `npub` or hex
public keys to `nostr.fips_trusted_raters`. This list defaults to empty and is
independent of social follows and the pubsub service roster. A rater supplies a
general prior about a peer; direct observations of the connection still apply.
The shared pubsub client manages bounded rating exchange and stops it when the
client shuts down. Reload the daemon after changing the list.

The service roster does not create a physical connection by itself. Establish
a path using LAN discovery, local rendezvous, existing peers, or explicit UDP
connection hints. Public relays and WebSocket bootstrap seeds can be disabled
with explicit empty `server.fips_relays` and
`server.fips_websocket_seed_urls` lists. Fresh devices still need a reachable
first peer; browser participation remains subject to available transports.

Application event validation and authorization remain at the endpoints. The
optional decentralized relay bridge still requires its build feature and
`nostr.decentralized_pubsub = true`. A node with Nostr disabled can remain a
plain FIPS transit router.

The daemon regression exercises root publication, root lookup, authorized
event ingestion, rejection of an unauthorized author, and relay outbound
delivery through a third FIPS endpoint with no pubsub client. Blob tests also
cover hop budgets, cancellation recovery, alternate providers, and cached
retrieval after the original provider leaves.
