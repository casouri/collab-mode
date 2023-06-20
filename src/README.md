# OT control algorithm

The algorithm we use is kind of a mix of POT and stop-and-wait. Each
op ([crate::op::FatOp]) has a global sequence number (g-seq), which
gives all ops a total ordering. The g-seq is assigned by the central
server as it receives ops from sites. That means freshly made, local
ops don’t have a g-seq before they are sent to the server and acked
back. This is the same as in POT.

Unlike POT, sites don’t send new ops before receiving ack for the
previously sent ops. This eliminates the need to keep a separate
transform path for every other site. Every site only needs to keep one
history, which is made of the globally sequenced ops (let it be L₁)
followed by local unsequenced ops (let it be L₂). Note that in this
history, every op o_i’s context C(o_i) is all of ops before it: C(o_i)
= C(o_(i-1)) ∪ o_(i-1)

Whenever a new local op is generated, it’s appended at the end of L₂.
Whene a site receives an op from the server, two things can happen: 1)
the op is an ack for an op this site had sent to the server, the site
simply pop it from L₂ and append it to L₁; 2) the op is a remote op,
the site symmetrically transforms the op against all ops in L₂, append
the transformed op to the end of L₁, and replace L₂ with the newly
transformed L₂.

Note that we assume the communication channel between sites and the
server is reliable: so that ops received from server has continuously
incrementing g-seq.

# Synchronization with the editor

To keep the synchronization between the editor and the collab process,
the editor has to block user input, send new ops to the collab
process, receive remote ops, apply remote ops, and resume handling
user input. The creation of new ops has to be synchronized with
everything else in the control algorithm. The editor don’t have to
pool the collab process constantly: the collab process can send the
editor a notification when remote ops arrive, and the editor can send
new ops to the collab process and receive remote ops.

On the collab process side, it don’t process ops received from the
server until the editor initiates a new round of
send-local-op-receive-remote-op. The collab process just stores newly
arrived op in a buffer, pretending they have not arrived yet. When the
editor blocks user input and sends local ops, the collab process
processes these local ops first, pretending that they happened before
ops from the server arrives. Then it process ops from the server, then
it sends transformed remote ops to the editor.

Let’s see what happens if we don’t buffer ops from server or don’t
block user input. Imagine a remote op o_r arrives, and the collab
process transforms it and put it in the history. In the meantime, the
user makes an edit o_1 in the editor, and the editor sends o_1 to the
collab process. The collab process can’t append it to the end of L₂:
Putting it to the end of L₂ implies that the o_1’s context is every op
in L₁ and L₂, but clearly o_r (which is at the end of L₁) is not in
o_1’s context.
