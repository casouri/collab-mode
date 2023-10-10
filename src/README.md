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

COT avoids both IP2 and IP3 in its control algorithm at the cost of
having abysmal asymptotic complexity, our algorithm does a couple
ad-hoc patchwork that are a lot cheaper and very simple. The downside
is that the result isn’t always intuitive. The documents are still
consistent with each other, just the order might change if two user
undo and redo at the same position. I think that’s reasonable since a)
usually people tend to stay out of the way of each other and don’t
edit at the exact same position, and b) as long as the consistency is
guaranteed, small ordering issues can be easily fixed by the user.

IP2 says sequentially transforming an operation A against another
operation B and its inverse I(B) should end up giving you o back. This
sounds trivially satisfiable but isn’t. An example: let A = del(0,
abc), B = del(0, abc). With a normal transform function, you’ll end up
with del(0, ""). To avoid IP2, you just need to make sure you never
transform ops sequentially against an op and then its inverse.

The other property, IP3 is a bit more complicated. In essence, it says
the transformed inverse of an op should be equal to the inverse of the
transformed op. The formal definition is as follows: let A and B be
two ops with the same context (so they can transform against each
other), let A’ = IT(A, B), B’ = IT(B, A), then IT(I(A), B’) should
equal to I(A’). Ie,

IT(I(A), IT(B, A)) = I(IT(A, B))

To avoid IP3, you need to make sure you never transform an inverse op
I(A) against an op B where B is concurrent (context-independent) to
A. Ie, don’t mingle inverse with concurrent ops together.

My patch works are two-fold. First, when a delete op is canceled by
another delete op when transforming, it saves the canceled part of the
edit; when it later transforms against an insert op that redoes that
edit, the edit is restored in the delete op. Example: del(0, "A")
transforms against del(0, "A"), ins(0, "A"). Normally we’ll end up
with del(0, ""), but with the restoration trick, we end up with del(0,
"A"). This is fairly important for maintaining intuitive undo
behavior.

A real-world example: when you type "{" in Emacs with bracket
completion on, Emacs first deletes the "{", then inserts "{}". So the
series of ops is

``` text
ins(0, {)
del(0, {)
ins(0, {)
ins(1, })
```

Without the restoration trick, the user wouldn’t be able to undo all
the operations, because the very first op, ins(0, {), will be inversed
to del(0, {), and will be canceled by later ops.

The second patchwork that I did is to skip inverse-pairs when
transforming operations, like COT does. But we can only skip those
pairs when transforming in the local history. It is still possible for
an op to be transformed with an op, then later to that op’s inverse.
This is called the decoupled-pair in the COT paper. Avoiding that is
too expensive, so we just don’t do it.

# Undo policy

To keep the user interface simple, we only allow undo and redo in a
linear history. Basically, the default undo/redo behavior anyone would
expect. Users can undo some operations, and redo them; but once they
make some original edit, the undone ops are “lost” and can’t be redone
anymore.

We also restrict undo and redo to ops created by the user, ie,
everyone can only undo their own edits.

# Undo implementation

To skip do-undo pairs, whenever we transform an op A against a series
of ops OPS, we scan OPS end to front and marks all the do-undo pairs,
and when transforming A against OPS, the marked ones are skipped.

To chose the correct op to undo, and only undo the ops the user
created, we maintain an "undo queue" (aka list of original ops made by
the user), which points to each original op in the history, and we
keep track of a "undo tip" that points into the undo queue, marking
the current undo progress. Undo will move the undo tip backward, and
redo will move it forward.

When the editor sends an Undo request, the collab process looks at the
undo queue and undo tip, picks the original op to undo, transforms it,
and sends it back to the editor. But at this point it doesn't modify
the undo queue or undo tip. It is when the editor receives the op,
applies it, and send it back, the collab process consider the undo
operation complete, and moves the undo tip. It also replaces the
pointer in the undo queue to point to the undo op just appended to the
history; so that when the next time when the user redo the op, the
redo op will be the inverse of this undo op.


[1] Conditions and Patterns for Achieving Convergence in OT-Based
Co-Editors

[2] https://people.apache.org/~al/wave_docs/ApacheWaveProtocol-0.4.pdf

[3] Context-Based Operational Transformation in Distributed
Collaborative Editing Systems
