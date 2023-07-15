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
infer the context of a global op with its g-seq, and that of a local
op with it’s position in L₂ plus the content of L₁.

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
process, receive remote ops, apply remote ops, and resume handling
user input. The creation of new ops (which happens on the editor) has
to be synchronized with everything else in the control algorithm
(which happens on the collab process). The editor doesn’t have to pull
the collab process constantly: the collab process can send the editor
a notification when remote ops arrive, and the editor can send new ops
(or an empty list of new ops when there isn’t any) to the collab
process and receive remote ops.

On the collab process side, it don’t process ops received from the
server until the editor initiates a new round of
send-local-op-receive-remote-op. The collab process just stores newly
arrived op in a buffer, pretending they have not arrived yet. When the
editor blocks user input and sends local ops, the collab process
processes these local ops first, pretending that they happened before
ops from the server arrives. Then it process ops from the server, then
it sends transformed remote ops to the editor.

Let’s see what happens if we don’t buffer ops from server or don’t
block user input. Imagine a remote op o\_r arrives, and the collab
process transforms it and put it in the history. In the meantime, the
user makes an edit o₁ in the editor, and the editor sends o₁ to the
collab process. The collab process can’t append it to the end of L₂:
Putting it to the end of L₂ implies that the o₁’s context is every op
in L₁ and L₂, but clearly o\_r (which is at the end of L₁) is not in
o₁’s context.

# Undo

According to the COT paper [3], there are two properties, IP2 and IP3,
that the transform function need to satisfy in order to archive
convergence with undo operations. They are like TP1 and TP2, namely,
you can either use a transform function that satisfies them, or use a
control algorithm that avoids them.

Most OT algorithm uses a transform function that satisfies TP1 and a
control algorithm that avoids TP2. COT does exactly that. For IP2 and
IP3, COT avoids both with its control algorithm, and our algorithm
does the same.

IP2 says sequentially transforming an operation o against another
operation o₁ and its inverse I(o₁) should end up giving you o back.
This sounds trivially satisfiable but isn’t. A trivial example: let o
= del(0, abc), o₁=del(0, abc). With a normal transform function,
you’ll end up with del(0, ""). To avoid IP2, you just need to make
sure you never transform ops sequentially against an op and its
inverse.

IP3 is a bit more complicated. In essence, it says transformed inverse
of an op should be equal to the inverse of transformed op. The formal
definition is this: let o₁ and o₂ be two ops with the same context (so
they can transform against each other), let o₁’ = IT(o₁, o₂), o₂’ =
IT(o₂, o₁), then IT(I(o₁), o₂’) should equal to I(o₁’). IOW,

IT(I(o₁), IT(o₂, o₁)) = I(IT(o₁, o₂))

To avoid IP3, you need to make sure you never transform an inverse op
I(o₁) against an op o₂ where o₂ is concurrent (context-independent) to
o₁. IOW, don’t mingle inverse with concurrent ops together.

To keep things simple, we only allow undo and redo on a linear
history. Basically, the default undo/redo behavior anyone would
expect. Users can undo some operations, and redo them; but once they
make some original edit, the undone ops are “lost” and can’t be redone
anymore.

We also restrict undo and redo to ops created by the user, ie, you can
only undo your own edits.

Alongside the global history, we keep track of a client history
([crate::engine::ClientHistory]). Ops in the client history are
ordered by the sequence at which they are applied on the editor. All
the ops apply one after another, so there is no concurrent ops, and
IP3 is avoided.

We also keep track of the local edits in the client history, these are
the ops the user can undo.

To undo/redo, the editor sends a Undo/Redo request, and if there are
available ops to undo/redo, the collab process sends back an op that
will perform the undo/redo. The editor should apply the op and send it
back with a SendOp request, so that the collab process knows that the
operation is applied.

When the collab process receives a local op that’s labeled undo, it
finds the corresponding original op in the editor history, and marks
that op as undone, and transforms ops after it as if this op has
become an identity op. Since we modify the ops in-place rather than
appending the inverse at the end, we avoids IP2.

If the op is labeled redo, we find the corresponding op, flip it back
from identify to the original op, and transform the ops after it.

Note that under our algorithm, we can’t freely redo any operation in
any order, we have to redo in the reverse order in which we undone the
ops. Because we want to ensure that when we redo an op and thus
flipping it back to the original op, there is no ops before it that
are undone. If there are, using the original op wouldn’t be correct.

A more flexible undo system that allows any undo and redo order (like
that of COT) is much more complicated to implement. I find the
limitation to be quite a reasonable trade-off.

[1] Conditions and Patterns for Achieving Convergence in OT-Based
Co-Editors

[2] https://people.apache.org/~al/wave_docs/ApacheWaveProtocol-0.4.pdf

[3] Context-Based Operational Transformation in Distributed
Collaborative Editing Systems
