# Note

To include private modules in the doc, run

```shell
cargo doc --document-private-items --open
```

# Design goal

I want collab-mode to be:
- Usable out-of-the-box, so we have builtin NAT traversal and run a
  signaling server (perhaps even a TURN server in the future?). In the
  future the editor frontend should auto-download binary and run it,
  etc, etc.
- Limited in scope, we focus on realtime, temporary collaboration. For
  non-realtime collaboration you have Git; for permanent realtime you
  have Google Docs. Collab-mode would be something that you use for
  quickly pair-program for a little bit, or collaborate on some notes.
- p2p, in the sense that I don’t need to maintain a server for other
  people to use collab-mode. (Well, I still need to set up a signaling
  server, but the load on a signaling server should be relatively
  light.)
- Simple in design and implementation, I’m willing to aggressively
  trade feature/efficiency for simplicity. Because correctness is so
  important for collaborative-editing, it’s important to keep things
  as simple as possible.
- Lean, meaning it has a small binary size and doesn’t use much
  memory. This one has lower priority but is still on the list.
- Stable and secure. These two are standard requirements, collab-mode
  isn’t special here.

# Topology

For each doc there is a server node that owns the file and is the
source-of-truth. Each client sends ops to the server and the server
broadcasts the op to all other clients. Clients join the group by
contacting server and get a copy of the document. Collab-mode is p2p
in the sense that every host is both a client and a server.

So we have a central server for each doc. Apart from the fact that our
control algorithm doesn’t support truly distributed gossip
propagation, I don’t really believe in distributed collab editing.
Like, what’s the point? To have any reasonable level of speed and
availability, you inevitably have to have some central server to
synchronize the messages and keep track of peers. At that point might
as well make it fully centralized for simpler design and better
performance.

# Connection and RPC

We use webrtc data channels to establish p2p connections. It needs a
public signaling server, which is implemented in
[crate::signaling::server] The collab server (“host”) registers with a
UUID on the signaling server, and collab client can connect to the
server by querying the signaling server.

For RPC, we implement a simple RPC ([crate::webrpc]) on top of webrtc
data channel.

# Communicating with editor

[crate::jsonrpc] provides a jsonrpc server that faces the editor. The
jsonrpc server creates a [crate::collab_doc::Doc] upon editor
request, by either sharing to the local server or connecting to a
remote server, and delgates editor requests to
[crate::collab_doc::Doc].

# OT control algorithm

The algorithm we use is kind of a mix of POT[1] and stop-and-wait[2].
Each op ([crate::op::FatOp]) has a global sequence number (g-seq),
which gives all ops a total ordering. The g-seq is assigned by the
central server as it receives ops from sites. That means freshly made,
local ops don’t have a g-seq before they are sent to the server and
acked back. This is the same as in POT.

Unlike POT, sites don’t send new ops before receiving ack for the
previously sent ops. This eliminates the need to keep a separate
transform path for every site in the group. Every site only needs to
keep one history ([crate::engine::GlobalHistory]), which is made of
the globally sequenced ops (let it be L₁) followed by local
unsequenced ops (let it be L₂). Note that in this history, every op
o\_i’s context C(o\_i) is the set of all ops before it, and we can
infer the context of a global op with its g-seq (every ops before it
in L₁), and that of a local op with it’s position in L₂ (every ops
before it in L₂ plus all ops in L₁).

At all times, the edit’s document state is the same as the document
state at the end of the history (L₁ + L₂). So we know the op we send
to the editor can be directly applied, and we know the context of the
ops send by the editor.

Whenever a new local op from the editor arrives, it’s appended at the
end of L₂. Whene a site receives an op from the server, two things can
happen: 1) the op is an ack for an op this site had sent to the
server, the site simply pop it from L₂ and append it to L₁; 2) the op
is a remote op, the site symmetrically transforms the op against all
ops in L₂, append the transformed op to the end of L₁, and replace L₂
with the newly transformed L₂, and send the transformed remote op to
the editor to apply.

Note that we assume the communication channel between sites and the
server is reliable, so that ops received from server has continuously
incrementing g-seq.

# Synchronization with the editor

To keep the synchronization between the editor and the collab process,
the editor has to block user input, send new ops to the collab
process, apply remote ops, and resume handling user input. The
creation of new ops (which happens on the editor) has to be
synchronized with everything else in the control algorithm (which
happens on the collab process). The editor doesn’t have to pull the
collab process constantly: the collab process can send the editor a
notification when remote ops arrive, and the editor can send new ops
(or an empty list of new ops when there aren’t any) to the collab
process and receive remote ops.

On the collab process side, it don’t process remote ops received from
the server until the editor initiates a new round of
send-local-op-process-remote-op. The collab process just stores newly
arrived op in a buffer, pretending they have not arrived yet. When the
editor blocks user input and sends local ops, the collab process
processes these local ops first, pretending that they happened before
ops from the server arrives. Then it process ops from the server, then
it sends transformed remote ops to the editor.

Let’s see what happens if we don’t buffer ops from server or don’t
block user input. Imagine a remote op R arrives, and the collab
process transforms it and put it in the history. In the meantime, the
user makes an edit A in the editor, and the editor sends A to the
collab process. The collab process can’t append it to the end of L₂:
Putting it to the end of L₂ implies that the A’s context is every op
in L₁ and L₂, but clearly R (which is at the end of L₁) is not in A’s
context.

# Undo mechanism

In the simplest term, to undo a op B in history [A B C], we compute
the inverse of B, I(B), treat it as an op that comes after B but
concurrent with all other ops after B in the history. So in this case
we transform I(B) against C, and gets the undo op that can be applied
to the document.

According to the COT paper [3], there are two properties, IP2 and IP3,
that the transform function need to satisfy in order to archive
convergence with undo operations. Like TP1 and TP2, you can either use
a transform function that satisfies them, or use a control algorithm
that avoids them.

IP2 says sequentially transforming an operation A against another
operation B and its inverse I(B) should end up giving you A back. This
sounds trivially satisfiable but isn’t. An example: let A = del(0,
abc), B = del(0, abc). With a normal transform function, you’ll end up
with del(0, ""). To avoid IP2, you just need to make sure you never
transform ops sequentially against an op and then its inverse (harder
than you think, because of de-coupled inverse pairs).

The other property, IP3 is a bit more complicated. In essence, it says
the transformed inverse of an op should be equal to the inverse of the
transformed op. The formal definition is as follows: let A and B be
two ops with the same context (so they can transform against each
other), let A’ = IT(A, B), B’ = IT(B, A), then IT(I(A), B’) should
equal to I(A’). Ie,

IT(I(A), IT(B, A)) = I(IT(A, B))

COT avoids both IP2 and IP3 in its control algorithm at the cost of
having abysmal asymptotic complexity: you need to always transform
from the original op, that means keeping the original context for each
op. Optimizations like transformation at the server, and using single
path of transformation are out of question.

After much struggle, I decided to use tombstones. They solves every
problem and is relatively simple, at the mere cost of keeping deleted
characters. collab-mode is for temporary realtime sharing, keeping
some deleted charaters is totally acceptable.

Since we have tombstones now, we need to convert between editor
positions (excludes tombstones) and internal positions (includes
tombstones). The conversion is sped up with cursors, basically the
trick used in Yjs [4]; we don’t need to use trees like ST-Undo [5]
does.

This part of the code ([crate::engine::InternalDoc]) is really the
part that I’m most unhappy with, it’s not very clean. In the future I
might revisit this and try to make the logic cleaner and with less
edge-cases.

# Undo policy

To keep the user interface simple, we only allow undo and redo in a
linear history. Basically, the default undo/redo behavior people
usually expect. Users can undo some operations, and redo them; but
once they make some original edit, the undone ops are “lost” and can’t
be redone anymore.

We also restrict undo and redo to ops created by the user, ie,
everyone can only undo their own edits.

# Undo op selection

To chose the correct op to undo, and only undo the ops the user
created, we maintain an "undo queue" (aka list of original ops made by
the user), which points to each original op in the history, and we
keep track of a "undo tip" that points into the undo queue, marking
the current undo progress. Undo will move the undo tip backward, and
redo will move it forward.

When the editor sends an Undo request, the collab process looks at the
undo queue and undo tip, picks the original op to undo, transforms it,
and sends it back to the editor. But at this point it doesn't modify
the undo queue or undo tip. The editor receives the op, applies it,
and send it back, then collab process consider the undo operation
complete, and moves the undo tip. It also replaces the pointer in the
undo queue to point to the undo op just appended to the history; so
that when the next time when the user redo the op, the redo op will be
the inverse of this undo op.

# Beta

Right now there’s some (potentially expensive and immediately
panicking) runtime checks in-place. Obviously we need to remember to
remove them later:
[crate::engine::InternalDoc::check_cursor_positions], some check
in [crate::engine::ServerEngine::convert_internal_op_and_apply].


[1] Conditions and Patterns for Achieving Convergence in OT-Based
Co-Editors

[2] <https://people.apache.org/~al/wave_docs/ApacheWaveProtocol-0.4.pdf>

[3] Context-Based Operational Transformation in Distributed
Collaborative Editing Systems

[4] <https://josephg.com/blog/crdts-go-brrr/>

[5] A semi-transparent selective undo algorithm for multi-user
collaborative editors
