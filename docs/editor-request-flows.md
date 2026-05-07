# Collab mode server - editor request flows design document

## Summary

The collab-mode server acts as a collaborative document editing system where the server serves as both a host for local documents and a client for remote documents. The server communicates with the editor through LSP-style JSON-RPC messages and with remote servers through WebChannel connections.

The server maintains two types of documents:
- **Local documents** (`docs`): Documents owned and hosted by this server
- **Remote documents** (`remote_docs`): Documents hosted by other servers that this server has opened

**Special case: Dual server/client role**
- When a server opens one of its own local documents (e.g., through OpenFile), it acts as both the server (host) and a client for that document
- The document exists in both `docs` (as the authoritative host) and `remote_docs` (as a client/subscriber)
- This dual role enables the server to edit its own documents using the same client interface
- Messages sent to self in this configuration are handled asynchronously to prevent deadlock

All editor requests follow a similar pattern:
1. Editor sends a request to the server
2. Server determines if the target is local or remote
3. For local targets, the server processes directly
4. For remote targets, the server forwards the request via WebChannel
5. Server sends response back to editor

## Key concepts

### Document types

**Doc** - A locally hosted document containing:
- `name`: Human-readable name
- `meta`: Metadata as JSON map
- `abs_filename`: PathId (either Buffer or Path)
- `file_desc`: FileDesc for identification
- `engine`: ServerEngine for operations
- `buffer`: GapBuffer containing the actual text
- `subscribers`: Remote peers subscribing to this doc
- `disk_file`: Optional file handle for saving

**RemoteDoc** - A document hosted by a remote server:
- `name`: Human-readable name
- `file_desc`: FileDesc for identification
- `engine`: ClientEngine for operations
- `buffer`: GapBuffer containing the synced text
- `remote_op_buffer`: Buffer for incoming remote operations
- `next_site_seq`: Local sequence number for operations

### Identifiers

**PathId** - Identifies a document's storage location:
- `Buffer(String)`: In-memory buffer with unique name
- `Path(PathBuf)`: File with absolute path

**EditorFileDesc** - How the editor references files:
```json
{
  "hostId": "server-id",
  "project": "project-name",
  "file": "relative/path/to/file.txt"
}
```

**FileDesc** - Internal file descriptor:
```json
// Project directory
{"type": "Project", "id": "project-name"}

// File in project
{"type": "ProjectFile", "project": "project-name", "file": "relative/path.txt"}
```

### Reserved projects

- `_buffers`: Virtual project for in-memory buffers
- `_files`: Virtual project for standalone files with absolute paths

### Next struct

The `Next` struct encapsulates response handling:
- `send_resp()`: Send response to editor
- `send_notif()`: Send notification to editor
- `send_to_remote()`: Forward message to remote server

## Editor request flows

### Initialize

Establishes the connection between editor and server.

**Request**
```json
{
  "method": "Initialize",
  "params": {}
}
```

**Response**
```json
{
  "hostId": "server-unique-id"
}
```

**Flow**
1. Editor sends Initialize request
2. Server returns its `host_id`
3. No errors possible for this request

### List projects

Lists all available projects from a server (local or remote).

**Request**
```json
{
  "method": "ListProjects",
  "params": {
    "hostId": "target-server-id"
  }
}
```

**Response**
```json
{
  "files": [
    {
      "file": {"hostId": "server-id", "project": "project1", "file": ""},
      "filename": "project1",
      "isDirectory": true,
      "meta": {}
    },
    {
      "file": {"hostId": "server-id", "project": "doc.txt", "file": "doc.txt"},
      "filename": "doc.txt",
      "isDirectory": false,
      "meta": {}
    }
  ]
}
```

**Flow**
1. Editor sends ListProjects request with target `host_id`
2. Server checks if `host_id` is local or remote
3. **If remote**:
   - Check connection status
   - Send `Msg::ListFiles { dir: None }` to remote via WebChannel
   - **Remote server processing**:
     - Lists all available projects
     - Lists standalone documents not in any project
     - Formats each as ListFileEntry with metadata
     - Sends back `Msg::FileList(files)` with `req_id`
   - **Local server receives response**:
     - Validates `req_id` matches pending request
     - Forwards files list to editor
4. **If local**:
   - List all projects from project registry
   - List docs not in any project (virtual `_files` project)
   - List docs in virtual `_buffers` project
   - Return ListFilesResp to editor

**Errors**
- `NotConnected`: If remote host is not connected
- `IoError`: If unable to read local filesystem

### List files

Lists files within a specific directory/project.

**Request**
```json
{
  "method": "ListFiles",
  "params": {
    "dir": {"hostId": "server-id", "project": "myproject", "file": "src"}
  }
}
```

**Response**
Same as ListProjects response format.

**Flow**
1. Editor sends ListFiles request with directory descriptor
2. Server checks if target is local or remote
3. **If remote**:
   - Convert EditorFileDesc to FileDesc
   - Send `Msg::ListFiles { dir: Some(`file_desc`) }` to remote
   - **Remote server processing**:
     - Converts FileDesc to filesystem path
     - Reads directory contents from disk
     - Formats entries as ListFileEntry items
     - Sends back `Msg::FileList(files)` or `Msg::ErrorResp` if error
   - **Local server receives response**:
     - If FileList: forwards to editor
     - If ErrorResp: converts to ResponseError and sends to editor
4. **If local**:
   - Convert FileDesc to filesystem path
   - Read directory contents from disk
   - Filter and format entries
   - Return ListFilesResp to editor

**Errors**
- `NotConnected`: If remote host is not connected
- `IoError`: If directory doesn't exist or permission denied

### Open file

Opens a file for editing, creating it if necessary.

**Request**
```json
{
  "method": "OpenFile",
  "params": {
    "file": {"hostId": "server-id", "project": "myproject", "file": "src/main.rs"},
    "mode": "Open" // or "Create"
  }
}
```

**Response**
```json
{
  "content": "file content here...",
  "siteId": 1,
  "filename": "main.rs",
  "file": {"hostId": "server-id", "project": "myproject", "file": "src/main.rs"}
}
```

**Flow**
1. Editor sends OpenFile request
2. Server checks if file is local or remote
3. **If local**:
   - Check if trying to open project directory (error)
   - Convert FileDesc to PathId
   - Check if already opened in `self.docs`
   - If not opened:
     - Read file from disk (or create if mode=Create)
     - Create new Doc with ServerEngine
     - Add to `self.docs` with new `doc_id`
     - Add ourselves as subscriber
     - Create corresponding RemoteDoc for local editing
     - Add to `self.remote_docs` and `remote_doc_id_map`
   - Return OpenFileResp with content and `site_id`
4. **If remote**:
   - Check if already opened in `self.remote_docs`
   - If already opened: return existing content
   - If not opened:
     - Send `Msg::RequestFile(`file_desc`, `mode`)` to remote
     - **Remote server processing**:
       - Checks if document already exists
       - If not exists and mode=Open: returns error
       - If not exists and mode=Create: creates new file
       - Creates document with engine if needed
       - Assigns new `site_id` to requester
       - Adds requester as subscriber
       - Creates snapshot with content, sequence number, and `site_id`
       - Sends back `Msg::Snapshot(snapshot)`
     - **Local server receives Snapshot**:
       - Creates RemoteDoc with assigned `site_id`
       - Initializes buffer with snapshot content
       - Stores in remote document registry
       - Sends OpenFileResp to editor
   - Return content and `site_id`

**Errors**
- `BadRequest`: If trying to open a project directory
- `IoError`: If file doesn't exist (mode=Open) or can't be created (mode=Create)
- `NotConnected`: If remote host is not connected
- `PermissionDenied`: If remote server denies access

### Make directory

Creates an empty directory inside a project on the target host.

**Request**
```json
{
  "method": "MakeDirectory",
  "params": {
    "file": {"hostId": "server-id", "project": "myproject", "file": "src/newdir"}
  }
}
```

**Response**
The server echoes the request back so the editor can confirm what was
created:
```json
{
  "file": {"hostId": "server-id", "project": "myproject", "file": "src/newdir"}
}
```

**Flow**
1. Editor sends MakeDirectory with the descriptor of the directory
   to create.
2. Server checks if target is local or remote.
3. **If local**:
   - Resolve the descriptor to a filesystem path under the project
     root.
   - Reject relative or escaping paths (`BadRequest`).
   - Check the calling peer has `create` permission.
   - `mkdir -p`-style create the directory.
   - Echo the descriptor back as the response.
4. **If remote**:
   - Forward as a remote request to the host that owns the project.
   - Remote performs the same check + create, sends a response back.
   - Local server forwards the response to the editor.

**Errors**
- `BadRequest`: Path is absolute, escapes the project, or names a
  reserved project (`_files`, `_buffers`).
- `IoError`: Filesystem error (parent missing, permission, etc.).
- `PermissionDenied`: Calling peer lacks `create` permission.
- `NotConnected`: Remote host is not connected.

### Share file

Creates a new shared document from content provided by the editor.

**Request**
```json
{
  "method": "ShareFile",
  "params": {
    "filename": "my-buffer.txt",
    "content": "initial content",
    "meta": {}
  }
}
```

**Response**
```json
{
  "file": {"hostId": "self", "project": "_buffers", "file": "my-buffer.txt"},
  "siteId": 0
}
```

**Flow**
1. Editor sends ShareFile request with filename and content
2. Server expands filename (handle `~` expansion)
3. Determine PathId based on filename:
   - Absolute path → `_files` project with PathId::Path
   - Relative path → `_buffers` project with PathId::Buffer
4. Check if file already exists in `self.docs`
5. Generate new `doc_id`
6. If absolute path exists, open file for read/write
7. Create new Doc with:
   - ServerEngine initialized with content length
   - GapBuffer populated with content
   - Metadata from request
8. Add Doc to `self.docs`
9. Create corresponding RemoteDoc for local editing
10. Add RemoteDoc to `self.remote_docs`
11. Return EditorFileDesc and `site_id`

**Errors**
- `BadRequest`: If file with same name already exists

### Send ops

Sends editing operations from the editor to apply to a document.

**Request**
```json
{
  "method": "SendOps",
  "params": {
    "ops": [
      {"op": {"Ins": [5, "hello"]}, "groupSeq": 1},
      {"op": {"Del": [3, "abc"]}, "groupSeq": 1}
    ],
    "file": {"hostId": "server-id", "project": "myproject", "file": "doc.txt"}
  }
}
```

**Response**
```json
{
  "ops": [
    {"op": {"Ins": [[5, "hello"]]}, "siteId": 1},
    {"op": {"Del": [[3, "abc"]]}, "siteId": 1}
  ],
  "lastGlobalSeq": 42,
  "pendingLocalOps": 0,
  "docLen": 1234
}
```

The response includes validation fields for the editor:
- `lastGlobalSeq`: Last global sequence number processed by the server
- `pendingLocalOps`: Number of pending local operations not yet sent to remote
- `docLen`: Current document length for validation

**Flow**
1. Editor sends SendOps with operations and file descriptor
2. Server finds RemoteDoc for the file
3. Drain any buffered remote operations
4. **Process local operations**:
   - For each EditorOp:
     - Assign sequence number
     - Convert to operation
     - Apply to local buffer
     - Collect operations for remote
5. **Send to remote** (if file is remote):
   - Package as ContextOps with context vector
   - Send `Msg::OpFromClient(`context_ops`)` to remote
   - **Remote server processing**:
     - Finds document by `doc_id`
     - Updates sender's sequence tracking
     - Transforms each operation for consistency
     - Applies operations to document buffer
     - Broadcasts transformed ops to all other subscribers
     - Each subscriber gets ops after their last received sequence
   - **Note**: No immediate response; ops arrive async via OpFromServer
6. **Process buffered remote operations**:
   - Transform each buffered operation
   - Apply to local buffer
   - Add to response list
7. **Prepare response**:
   - Collect all edit instructions transformed from remote ops
   - Include last sequence number
8. Return SendOpsResp to editor

**Errors**
- `IoError`: If file not found in `remote_docs`

### Undo

Generates undo/redo operations for a document.

**Request**
```json
{
  "method": "Undo",
  "params": {
    "file": {"hostId": "server-id", "project": "myproject", "file": "doc.txt"},
    "kind": "Undo" // or "Redo"
  }
}
```

**Response**
```json
{
  "ops": [
    {"Ins": [[5, "deleted text"]]},
    {"Del": [[10, "inserted text"]]}
  ]
}
```

**Flow**
1. Editor sends Undo request
2. Server finds RemoteDoc for the file
3. Based on kind:
   - Undo: Call `engine.generate_undo_op()`
   - Redo: Call `engine.generate_redo_op()`
4. Engine returns EditInstructions
5. Return UndoResp with operations

**Note**: The editor must apply these operations locally and send them back via SendOps for synchronization.

**Errors**
- `InternalError`: If RemoteDoc not found

### Print history

Returns a human-readable representation of the operation history for debugging.

**Request**
```json
{
  "method": "PrintHistory",
  "params": {
    "file": {"hostId": "server-id", "project": "myproject", "file": "doc.txt"},
    "debug": false
  }
}
```

**Response**
```json
{
  "history": "(A printed history)"
}
```

**Flow**
1. Editor sends PrintHistory request with file descriptor and debug flag
2. Server finds RemoteDoc for the file
3. Call `engine.print_history(debug)` method
4. Return PrintHistoryResp with history string

**Note**: This is primarily a debugging tool. The history string format may change between versions.

**Errors**
- `IoError`: If RemoteDoc not found

### Move file

Moves or renames a file within a project.

**Request**
```json
{
  "method": "MoveFile",
  "params": {
    "hostId": "server-id",
    "project": "myproject",
    "oldPath": "src/old.rs",
    "newPath": "src/new.rs"
  }
}
```

**Response**
```json
{
  "hostId": "server-id",
  "project": "myproject",
  "oldPath": "src/old.rs",
  "newPath": "src/new.rs"
}
```

**Flow**
1. Editor sends MoveFile request
2. **If remote**:
   - Send `Msg::MoveFile(project, old_path, new_path)` to remote
   - **Remote server processing**:
     - Validates project exists
     - Builds full filesystem paths
     - Performs filesystem rename operation
     - Scan through opened docs and find affected ones by their absolute path
     - Updates affected document's path and descriptor
     - Gets list of subscribers
     - Sends `Msg::FileMoved` response with `req_id`
     - Sends `Msg::FileMoved` notification to all subscribers
   - **Local server receives response**:
     - Receives `Msg::FileMoved` with `req_id`
     - Sends MoveFileResp to editor
3. **If local**:
   - Perform same operations as remote (validate, rename, update)
   - Send MoveFileResp to editor
   - Send `Msg::FileMoved` notification to all subscribers

**Errors**
- `NotConnected`: If remote host not connected
- `IoError`: If move operation fails (permission, file not found, etc.)

### Save file

Saves a document to disk.

**Request**
```json
{
  "method": "SaveFile",
  "params": {
    "file": {"hostId": "server-id", "project": "myproject", "file": "doc.txt"}
  }
}
```

**Response**
```json
{
  "file": {"hostId": "server-id", "project": "myproject", "file": "doc.txt"}
}
```

**Flow**
1. Editor sends SaveFile request
2. **If remote**:
   - Find `doc_id` from remote documents
   - Send `Msg::SaveFile(`doc_id`)` to remote
   - **Remote server processing**:
     - Finds document by `doc_id`
     - Collects buffer content as string
     - Writes to disk
     - Sends `Msg::FileSaved(`doc_id`)` response
     - Or `Msg::ErrorResp(IoError)` if save fails
   - **Local server receives response**:
     - Receives `Msg::FileSaved` with `req_id`
     - Finds RemoteDoc to get file descriptor
     - Sends SaveFileResp to editor
3. **If local**:
   - Find document by PathId
   - Perform save operation directly
   - Return SaveFileResp or error to editor

**Errors**
- `NotConnected`: If remote host not connected
- `IoError`: If file not open, write fails, or no `disk_file` handle

### Close file

Closes a local document, saving it to disk and notifying subscribers.

**Request**
```json
{
  "method": "CloseFile",
  "params": {
    "file": {"hostId": "server-id", "project": "myproject", "file": "doc.txt"}
  }
}
```

**Response**
```json
{
  "file": {"hostId": "server-id", "project": "myproject", "file": "doc.txt"}
}
```

**Note**: CloseFile uses the `DeleteFileParams` type for its request parameters (same structure as DeleteFile).

**Flow**
1. Editor sends CloseFile request
2. **Local files only**:
   - Server checks if `host_id` matches local server
   - If remote: returns `BadRequest` error
   - If local:
     - Converts EditorFileDesc to FileDesc to PathId
     - Finds document in `self.docs` by matching PathId
     - If not found: returns `IoError`
     - If found:
       - Saves document to disk (logs error if fails, but continues)
       - Collects list of subscribers (including local server)
       - Removes document from `self.docs`
       - Sends `Msg::FileClosed` to each remote subscriber
       - Sends FileClosed notification to local editor
       - Returns CloseFileResp to editor

**Errors**
- `BadRequest`: If trying to close a remote file (CloseFile only works on local files)
- `IoError`: If file not open or save fails

**FileClosed notification**

When a file is closed on a remote server, subscribers receive a FileClosed notification:

```json
{
  "method": "FileClosed",
  "params": {
    "file": {"hostId": "server-id", "project": "myproject", "file": "doc.txt"}
  }
}
```

**Note**: CloseFile only applies to local documents. To disconnect from a remote document, use DisconnectFromFile instead.

### Delete file

Deletes a file or directory.

**Request**
```json
{
  "method": "DeleteFile",
  "params": {
    "file": {"hostId": "server-id", "project": "myproject", "file": "old.txt"}
  }
}
```

**Response**
```json
{
  "file": {"hostId": "server-id", "project": "myproject", "file": "old.txt"}
}
```

**Flow**
1. Editor sends DeleteFile request
2. **If remote**:
   - Convert EditorFileDesc to FileDesc
   - Send `Msg::DeleteFile(`file_desc`)` to remote
   - **Remote server processing**:
     - Converts FileDesc to filesystem path
     - Checks if path is file or directory
     - For directory, find all the opened docs under it, collect all the subscribers of contained doc; for file, collect its subscribers too
     - Deletes from filesystem
     - If doc exists for this file/directory:
       - Removes itself and contained docs (if directory) from registry
       - Sends `Msg::FileDeleted` notification to each subscriber we collected earlier
     - Sends `Msg::FileDeleted` response to the original caller
     - Or `Msg::ErrorResp(IoError)` if delete fails
   - **Local server receives response**:
     - Receives `Msg::FileDeleted` with `req_id`
     - Sends DeleteFileResp to editor
3. **If local**:
   - Convert to filesystem path
   - Perform same delete operations as remote
   - Send DeleteFileResp to editor
   - Send `Msg::FileDeleted` notifications to subscribers

**Errors**
- `NotConnected`: If remote host not connected
- `IoError`: If delete operation fails
- `PermissionDenied`: If insufficient permissions

### Disconnect from file

Closes a remote document and stops receiving updates.

**Request**
```json
{
  "method": "DisconnectFromFile",
  "params": {
    "file": {"hostId": "remote-server", "project": "project", "file": "doc.txt"}
  }
}
```

**Response**
```json
{}
```

**Flow**
1. Editor sends DisconnectFromFile request
2. Find RemoteDoc for the file
3. Remove from remote documents registry
4. Remove from file descriptor mapping
5. Send `Msg::StopSendingOps(`doc_id`)` to remote host
   - **Remote server processing**:
     - Finds document by `doc_id`
     - Removes sender from subscribers list
     - No response sent back
   - **Note**: This is fire-and-forget notification
6. Return empty response to editor

**Note**: This only applies to remote documents. Local documents remain open until explicitly deleted.

**Errors**
- `NotConnected`: If remote host not connected

### Declare projects

Declares new projects to be managed by the server.

**Request**
```json
{
  "method": "DeclareProjects",
  "params": {
    "projects": [
      {"name": "project1", "path": "~/Documents/project1"},
      {"name": "project2", "path": "/absolute/path/to/project2"}
    ]
  }
}
```

**Response**
```json
{}
```

**Flow**
1. Editor sends DeclareProjects request
2. Check for reserved project names (`_buffers`, `_files`)
3. Expand project paths:
   - Handle `~` expansion
   - Convert to absolute paths
4. For each project:
   - Create Project struct with name, root path, and empty metadata
   - Add to `self.projects` HashMap
5. Return empty response

**Errors**
- `BadRequest`: If using reserved project names
- `IoError`: If path expansion or canonicalization fails

### Update config

Updates server configuration for trusted hosts and permissions.

**Request**
```json
{
  "method": "UpdateConfig",
  "params": {
    "config": {
      "trustedHosts": {
        "host1": "cert-hash-abc123",
        "host2": "cert-hash-def456"
      },
      "permission": {
        "host1": {"write": true, "create": true, "delete": false},
        "host2": {"write": true, "create": false, "delete": false}
      }
    }
  }
}
```

**Permission object**
- `write`: Allow remote to send operations to documents
- `create`: Allow remote to create new files
- `delete`: Allow remote to delete files

**Response**
```json
{}
```

**Flow**
1. Editor sends UpdateConfig request with full config object
2. Replace `config.trusted_hosts` with provided `trustedHosts`
3. Replace `config.permission` with provided `permission`
4. Save updated configuration to disk
5. Return empty response

**Errors**
- `IoError`: If configuration save fails

## Asynchronous message flows

### Remote operation delivery

When a remote server sends operations for a document, they arrive asynchronously:

**Flow**:
1. Remote server has new ops for a document (from another client)
2. Remote sends `Msg::OpFromServer { doc, ops }` to all subscribers
3. **Local server receives**:
   - Finds RemoteDoc by `host_id` and `doc_id`
   - Adds ops to remote operation buffer
   - Sends `RemoteOpsArrived` notification to editor
4. **Editor processes**:
   - Editor receives notification
   - Editor sends `SendOps` request (possibly with empty ops)
   - Server processes buffered ops and returns them

This buffering mechanism ensures ops arrive in order and are processed atomically with local edits.

## Notification flows

### Connection management

**Accept connection**
```json
{
  "method": "AcceptConnection",
  "params": {
    "addr": "wss://signaling.server/path",
    "mode": "TrustedOnly" // Optional: "All" or "TrustedOnly" (default)
  }
}
```
Starts accepting connections on the specified signaling server.
`AcceptConnection` is signaling-server-only — the SSH/envoy transport
doesn't bind to anything.

**Accept connection flow**
1. Editor sends AcceptConnection notification with signaling address and optional accept mode
2. Server checks if already accepting on this signaling address:
   - If yes and already bound:
     - Updates accept mode if provided
     - Resets reconnect backoff
     - If mode is `All`, spawns task to revert to `TrustedOnly` after 3 minutes
     - Sends `AcceptingConnection` notification and returns
   - If no: Continues with accept process
3. Server adds signaling address to `self.active_signaling` HashMap with `SignalingState`
4. Server calls `signaling_channel.bind()` to bind to signaling server
5. On successful bind, server sends `AcceptingConnection` notification to editor
6. If mode is `All`, spawns background task to revert to `TrustedOnly` after 3 minutes
7. If connection breaks for various reasons:
   - Connection to signaling server breaks
   - Allocated signaling time expires
   - Host ID is already taken on that server
8. Server removes the signaling address from `self.active_signaling` HashMap and sends AcceptStopped notification to editor

**Accept mode**
- `All`: Accept connections from any host (auto-reverts to `TrustedOnly` after 3 minutes)
- `TrustedOnly`: Accept only connections from hosts in trusted_hosts (default)

**Accepting connection notification**

```json
{
  "method": "AcceptingConnection",
  "params": {
    "addr": "wss://signaling.server/path",
    "mode": "TrustedOnly"
  }
}
```

**Accept stopped notification**

```json
{
  "method": "AcceptStopped",
  "params": {
    "signalingAddr": "wss://signaling.server/path",
    "reason": "Signaling server closed the connection"
  }
}
```

**Stop accepting**
```json
{
  "method": "StopAccepting",
  "params": {
    "addr": "wss://signaling.server/path"
  }
}
```
Stops accepting connections on the specified signaling server. The server will:
1. Remove the signaling address from `self.active_signaling`
2. Send `AcceptStopped` notification with reason "Stopped accepting by user request"

**Set accept mode**
```json
{
  "method": "SetAcceptMode",
  "params": {
    "addr": "wss://signaling.server/path",
    "mode": "All" // or "TrustedOnly"
  }
}
```
Changes the accept mode for an active signaling server connection. The server will:
1. Update the accept mode in `active_signaling`
2. Send `AcceptModeChanged` notification if mode changed
3. If new mode is `All`, spawn task to revert to `TrustedOnly` after 3 minutes

**Accept mode changed notification**

```json
{
  "method": "AcceptModeChanged",
  "params": {
    "addr": "wss://signaling.server/path",
    "mode": "All"
  }
}
```

**Connect**
```json
{
  "method": "Connect",
  "params": {
    "hostId": "remote-server",
    "transportConfig": {
      "SCTP": { "signalingAddr": "wss://signaling.server/path" }
    }
  }
}
```

Initiates connection to a remote server. `transportConfig` is a tagged
enum carrying all per-peer connection info; it should be one of:

- `{ "SCTP": { "signalingAddr": ADDR } }` — connect via the
  signaling server at ADDR. The host must already be bound on that
  signaling server (the editor sends `AcceptConnection` first on
  its own end).
- `{ "SshStdio": { "sshHost": "user@host", "command": [...],
   "projects": [...] } }` — “envoy mode”. The collab process spawns
  `ssh user@host -- COMMAND...`; the remote command is expected to
  end up running `collab-mode envoy`. `command` is an array (program
  + args, each will be shell-escaped by server). `projects` is the list of
  `{name, path}` the editor wants the envoy to expose; the envoy
  adopts that list verbatim and trusts the spawning host with full
  read/write/delete permissions for that session. The envoy uses an
  ephemeral key/cert provisioned by the host, so no per-peer trust
  setup is required.

After receiving a Connect notification, the server will always send a
**Connected** notification to indicate the connection status:
- If the remote is already connected, the Connected notification is sent immediately.
- If the remote is not yet connected, the Connected notification is sent after the connection is established.

**Connected notification**
```json
{
  "method": "Connected",
  "params": {
    "hostId": "remote-server",
    "message": "Welcome message from remote server"
  }
}
```

**Connection state**
```json
{
  "method": "ConnectionState",
  "params": {}
}
```
Returns the current connection states for all active remote servers.

**Response**
```json
{
  "connections": [
    {
      "hostId": "remote-server-1",
      "state": "Connected"
    },
    {
      "hostId": "remote-server-2",
      "state": "Connecting"
    }
  ],
  "accepting": [
    {
      "addr": "wss://signaling.server/path1",
      "mode": "TrustedOnly"
    },
    {
      "addr": "wss://signaling.server/path2",
      "mode": "All"
    }
  ],
  "live": [
    {
      "file": {"hostId": "my-server", "project": "project1", "file": "doc.txt"},
      "subscribers": ["remote-server-1", "remote-server-2"],
      "filename": "document.txt",
      "meta": {"author": "user1", "created": "2024-01-01"},
      "seq": 42
    }
  ],
  "connected": [
    {
      "file": {"hostId": "remote-server-1", "project": "project2", "file": "file.md"},
      "filename": "readme.md",
      "meta": {"version": "1.0"}
    }
  ],
  "projects": [
    {
      "name": "project1",
      "path": "/absolute/path/to/project1"
    }
  ],
  "trustedHosts": {
    "remote-server-1": "cert-hash-abc123"
  },
  "permission": {
    "remote-server-1": {"write": true, "create": true, "delete": false}
  },
  "certHash": "my-cert-hash-xyz789"
}
```

**Flow**
1. Editor sends ConnectionState request
2. Server iterates through all active_remotes
3. For each active remote, creates a ConnectionStateEntry with:
   - `hostId`: The remote server's ID
   - `state`: Current connection state (Connected/Connecting/Disconnected/Fatal)
4. Server collects all signaling addresses where it's currently accepting connections from `self.active_signaling` HashMap (only those with `Bound` state)
5. For each accepting signaling address, creates AcceptingEntry with:
   - `addr`: The signaling server address
   - `mode`: The accept mode for this address (`All` or `TrustedOnly`)
6. Server iterates through all local documents in `self.docs`:
   - Creates LiveDocEntry for each document with:
     - `file`: EditorFileDesc for the document
     - `subscribers`: List of remote servers subscribed to this document
     - `filename`: Human-readable name of the document
     - `meta`: Document metadata
     - `seq`: Current sequence number from the engine
7. Server iterates through all remote documents in `self.remote_docs`:
   - Creates ConnectedDocEntry for each remote document with:
     - `file`: EditorFileDesc for the document
     - `filename`: Human-readable name of the document
     - `meta`: Document metadata
8. Server collects all declared projects from configuration
9. Returns ConnectionStateResp with:
   - `connections`: Array of connection entries for active remote connections
   - `accepting`: Array of accepting entries with address and mode
   - `live`: Array of locally hosted documents that may have subscribers
   - `connected`: Array of remote documents this server has opened
   - `projects`: Array of declared projects with their names and absolute paths
   - `trustedHosts`: Map of trusted host IDs to their certificate hashes
   - `permission`: Map of host IDs to their permission settings
   - `certHash`: This server's own certificate hash

**State values**
- `Connected`: Connection established and active
- `ConnectingStage1`: Sent connect message to remote, waiting for response
- `ConnectingStage2`: Received connect message from remote, establishing connection
- `Pending`: Waiting for signaling server connection before attempting to connect
- `Disconnected`: Connection was established but broke, will auto-reconnect
- `FailedToConnect`: Connection was never established, won't auto-reconnect

**Accept mode values**
- `All`: Accept connections from any host
- `TrustedOnly`: Accept only connections from hosts in trusted_hosts

### Send info

Sends metadata/information about a document to remote subscribers.

**SendInfo notification (from editor to server)**
```json
{
  "method": "SendInfo",
  "params": {
    "info": {"cursor": 42, "selection": [10, 20]},
    "file": {"hostId": "server-id", "project": "myproject", "file": "doc.txt"}
  }
}
```

**InfoReceived notification (from server to editor)**
```json
{
  "method": "InfoReceived",
  "params": {
    "info": {"cursor": 42, "selection": [10, 20]},
    "file": {"hostId": "server-id", "project": "myproject", "file": "doc.txt"},
    "sender": "sending-host-id"
  }
}
```

**Flow**
1. Editor sends SendInfo notification with metadata and file
2. **If file is remote**:
   - Get `doc_id` and `site_id` from RemoteDoc
   - Create Info message with:
     - `doc_id`: Document identifier
     - `sender`: This server's `host_id` (the actual sender, not the file's host)
     - `value`: Serialized info data
   - Send `Msg::InfoFromClient(info)` to remote host
   - **Remote server processing**:
     - Receives info containing `doc_id`, `sender` host ID, and value
     - Broadcasts info to all subscribers of the document
     - Sends `Msg::InfoFromServer(info)` to each subscriber
     - Automatically excludes the original sender using `info.sender` field
   - **Other clients receive**:
     - Receive `Msg::InfoFromServer` notification
     - Match `doc_id` to their remote documents
     - Send InfoReceived notification to their editor with `sender` field
3. **If file is local**:
   - Find document by PathId
   - Create Info message with:
     - `doc_id`: Document identifier
     - `sender`: This server's `host_id`
     - `value`: Serialized info data
   - Broadcast `Msg::InfoFromServer(info)` to all subscribers
   - Automatically excludes ourselves using `info.sender` field
4. **Special case: Self-messaging**:
   - When a document is both local and remote (server acts as both host and client for its own document), the server exists in both `docs` and `remote_docs`
   - Messages sent to self are handled asynchronously via a spawned task to prevent deadlock
   - The sender exclusion logic ensures you don't receive your own info updates

**Note**: SendInfo is a notification, not a request. No response is sent back to the editor.

## Reconnect tick handler

The server runs a periodic tick handler every 2 seconds that manages connection timeouts and automatic reconnection. This is the only place where timeouts are enforced—editor requests have no server-side timeout (the editor handles its own timeouts).

### Connection timeouts

The tick handler checks for stalled connections and times them out:

**Remote connection timeouts**
| State | Timeout | Action |
|-------|---------|--------|
| `ConnectingStage1` | 10 seconds | Mark as `FailedToConnect`, send `ConnectionBroke` notification |
| `ConnectingStage2` | 30 seconds | Mark as `FailedToConnect`, send `ConnectionBroke` notification |

**Signaling server timeouts**
| State | Timeout | Action |
|-------|---------|--------|
| `Binding` | 10 seconds | Mark as `FailedToBind`, remove from signaling channel |

### Automatic reconnection

The tick handler attempts to reconnect disconnected connections using exponential backoff.

**Remote reconnection**

Reconnection is attempted for remotes in these states:
- `Disconnected`: Connection was established but broke (will reconnect)
- `ConnectingStage1`: Keep retrying until we get a response

Reconnection is NOT attempted for:
- `Pending`: Waiting for signaling server to be ready
- `ConnectingStage2`: Already establishing connection
- `Connected`: Already connected
- `FailedToConnect`: Initial connection failed (won't retry)

**Signaling server reconnection**

Unlike remote connections, signaling servers always attempt reconnection even after initial failure. Reconnection is attempted for:
- `Disconnected`: Connection broke
- `FailedToBind`: Initial bind failed

Reconnection is NOT attempted for:
- `Bound`: Already connected
- `Binding`: Currently attempting to bind

### Exponential backoff

Both remote and signaling reconnections use exponential backoff, first 1s then doubles each time, maxing out at 256 seconds.

Backoff is reset when:
- Connection succeeds (mark as connected/bound)
- Connection is marked as disconnected (to allow quick first retry)

### Reconnection flow

```
[ConnectingStage1]
       │
       ├─(10s timeout)→ [FailedToConnect]
       │
 (remote responds)
       │
       ↓
[ConnectingStage2]
       │
       ├─(30s timeout)→ [FailedToConnect]
       │
 (connection established)
       │
       ↓
  [Connected]
```

### Notifications during reconnection

| Event | Notification |
|-------|--------------|
| Attempting reconnection | `Connecting` with `hostId` |
| Connection timed out | `ConnectionBroke` with `hostId` and `reason` |
| Connection established | `Connected` with `hostId` and `message` |

## Error handling

### Error codes

The server uses the following error codes in responses:

| Code | Name | Description |
|------|------|-------------|
| -32700 | ParseError | Invalid JSON in request |
| -32600 | InvalidRequest | Request structure invalid |
| -32601 | MethodNotFound | Unknown request method |
| -32602 | InvalidParams | Invalid parameters for method |
| -32603 | InternalError | Unexpected server error |
| -32002 | NotInitialized | Initialize not called first |
| 103 | DocFatal | Document corruption, must recreate |
| 104 | PermissionDenied | Access denied by remote server |
| 105 | IoError | File system operation failed |
| 113 | BadRequest | Invalid request parameters |
| 114 | NotConnected | Remote host not connected |

### Error response notification

The `ErrorResponse` notification is sent to the editor when errors occur during document operations.

**Notification format**
```json
{
  "method": "ErrorResponse",
  "params": {
    "code": 103,  // Error code (e.g., 103 for DocFatal)
    "file": {"type": "ProjectFile", "project": "myproject", "file": "doc.txt"},  // Optional FileDesc (not EditorFileDesc)
    "message": "Document fatal error: sequence mismatch"
  }
}
```

**Note**: The `file` field uses `FileDesc` format (without `hostId`), not `EditorFileDesc`. It can be null if the error is not associated with a specific file.

**When it's sent**
- **DocFatal errors** (code 103): When the document state becomes corrupted.
  - OT engine errors during operation transformation.
  - Sequence number mismatches between client and server.
  - Operations out of range for document length.
- **Remote operation failures**: When request to remote server fails.
- **IO errors**: When file operations fail on remote servers.

### Error response format

```json
{
  "id": "request-id",
  "error": {
    "code": 105,
    "message": "File not found: /path/to/file.txt",
    "data": null
  }
}
```
