## Transcript Test Format

Each transcript starts with a header section that’s similar to HTTP:
each line is a key-value pair, and the section ends with two newlines
(we use just newline, not \r\n). Right now there’s only one recognized
header key, Name.

The body is made of commands, each line is a command. Most commands
starts with `E<n>` (eg, `E1`), which means run this command on editor
`n`. There’s a special command `===CHECK===` that doesn’t start with
`E<n>` because it applies to all editors. Available commands:
- `E<n> MOVE <offset>`: Move cursor by `offset`. Positive offset moves
  forward, negative backward. `offset` can exceed document range, in
  that case we just move the cursor as far as possible.
- `E<n> INSERT <content>`: Insert content at current cursor position.
  `\t` and `\n` in `content` are converted to tab and newline.
  `content` itself shouldn’t contain newlines and tabs.
- `E<n> DELETE <count>`: Delete `count` characters. Delete backwards
  if `count` is positive, forward if negative. `count` can exceed the
  mock document range. In that case, just delete the available chars.
- `E<n> SEND`: Send the pending ops to server, and apply the returned
  remote ops. Insert and delete commands don’t send the ops to server
  immediately. Instead they apply the edit to the mock document and
  save the op to a pending op queue. Undo and redo commands sends the
  op immediately. When sending ops, all the ops have the same group
  seq. Group seq increments each time we send ops to server.
- `E<n> UNDO` & `E<n> REDO`: Undo/redo. The mock editor will first
  send all pending ops to server, apply the remote ops, then send an
  undo/redo request to server, apply the returned ops, and finally
  send the undo/redo op back to server.
- `===CHECK===`: For every editor, send any pending ops, apply the
  returned remote op, and check if all the editors have the same
  content in their mock document. This command doesn’t have to be the
  last command. Tests should interleave it in the middle of
  transcripts.
