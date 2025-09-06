# Implementation details

## Connection error handling

When we create a connection, we spawn a thread that listens for
incoming messages (`handle_incoming_messages`), and spwn a thread that
sends messages out (`handle_outgoing_messages`). When sending
messages, we ignore any connection broke error. Connection-breakage is
detected in the thread that listen for incoming messages; when that
happens, webchannel sends a ConnectionBroke message to ourself to
handle.

## Reconnect handling

When handling connect request, server adds a entry to
`active_remotes`: state is `Connecting`, `next_reconnect_time` is set
to in 30s. If connection succeeded (receives `Hey` message), server
sets state to `Connected`. If connection failed (receives
`ConnectionBroke` message), server sets state to `Disconnected` and
sets `next_reconnect_time` to appropriate time. The reconnect timer
(in `server::run`) runs every 2s and goes over each connection in
`active_remotes`, if a connection is in `Disconnect` state and
`next_reconnect_time` is reached, it tries to reconnect. The reconnect
time is doubled every time, from 1s all the way to 256s.
