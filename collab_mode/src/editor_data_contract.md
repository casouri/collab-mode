# Editor-Server Data Contract

## Request & response types

All the messages are in `src/message.rs`. Later I’ll generate a
documentation with Typescript style type definition for request and
response bodies.


## Request & repsonse

In collab server, most requests are served async, so editor sending
the request should expect the response asynchronously too. It’s also
possible to never receive a response of a request: when there’s a
disconnect, collab server doesn’t try to return a disconnect error
response, instead it just sends a disconnect notification; when the
remote server never responds with needed info, our collab server will
naturally never send the response to editor.

All the response objects contain enough context that editor doesn’t
need to keep a record mapping request id to context.
