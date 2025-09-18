# Collab Mode Server - Editor Request Flows Design Document

## Summary

The collab-mode server acts as a collaborative document editing system where the server serves as both a host for local documents and a client for remote documents. The server communicates with the editor through LSP-style JSON-RPC messages and with remote servers through WebChannel connections.

The server maintains two types of documents:
- **Local documents** (`docs`): Documents owned and hosted by this server
- **Remote documents** (`remote_docs`): Documents hosted by other servers that this server has opened

All editor requests follow a similar pattern:
1. Editor sends a request to the server
2. Server determines if the target is local or remote
3. For local targets, the server processes directly
4. For remote targets, the server forwards the request via WebChannel
5. Server sends response back to editor

## Key Concepts

### Document Types

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

### Reserved Projects

- `_buffers`: Virtual project for in-memory buffers
- `_files`: Virtual project for standalone files with absolute paths

### Next Struct

The `Next` struct encapsulates response handling:
- `send_resp()`: Send response to editor
- `send_notif()`: Send notification to editor
- `send_to_remote()`: Forward message to remote server

## Editor Request Flows

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

### ListProjects

Lists all available projects from a server (local or remote).

**Request**
```json
{
  "method": "ListProjects",
  "params": {
    "hostId": "target-server-id",
    "signalingAddr": "wss://signaling.server/path"
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

### ListFiles

Lists files within a specific directory/project.

**Request**
```json
{
  "method": "ListFiles",
  "params": {
    "dir": {"hostId": "server-id", "project": "myproject", "file": "src"},
    "signalingAddr": "wss://signaling.server/path"
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

### OpenFile

Opens a file for editing, creating it if necessary.

**Request**
```json
{
  "method": "OpenFile",
  "params": {
    "fileDesc": {"hostId": "server-id", "project": "myproject", "file": "src/main.rs"},
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

### ShareFile

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

### SendOps

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
  "lastSeq": 42
}
```

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

### MoveFile

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

### SaveFile

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

### DeleteFile

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

### DisconnectFromFile

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

### DeclareProjects

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

### UpdateConfig

Updates server configuration for accept mode and trusted hosts.

**Request**
```json
{
  "method": "UpdateConfig",
  "params": {
    "acceptMode": "Auto", // Optional: "Auto", "Manual", or "Disabled"
    "addTrustedHosts": { // Optional
      "host1": "cert-hash-abc123",
      "host2": "cert-hash-def456"
    },
    "removeTrustedHosts": ["host3", "host4"] // Optional
  }
}
```

**Response**
```json
{}
```

**Flow**
1. Editor sends UpdateConfig request
2. Get current configuration
3. If `acceptMode` provided:
   - Update `config.accept_mode`
   - Update shared Arc<Mutex<AcceptMode>>
4. If `addTrustedHosts` provided:
   - Add each host/cert pair to config.trusted_hosts
   - Update shared Arc<Mutex<HashMap>>
5. If `removeTrustedHosts` provided:
   - Remove each host from config.trusted_hosts
   - Update shared Arc<Mutex<HashMap>>
6. Save updated configuration to disk
7. Return empty response

**Errors**
- None (configuration updates are best-effort)

## Asynchronous Message Flows

### Remote Operation Delivery

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

## Notification Flows

### Connection Management

**AcceptConnection**
```json
{
  "method": "AcceptConnection",
  "params": {
    "signalingAddr": "wss://signaling.server/path",
    "transportType": "WebRTC"
  }
}
```
Starts accepting connections on the specified signaling server.

**Connect**
```json
{
  "method": "Connect",
  "params": {
    "hostId": "remote-server",
    "signalingAddr": "wss://signaling.server/path",
    "transportType": "WebRTC"
  }
}
```
Initiates connection to a remote server.

### SendInfo

Sends metadata/information about a document to remote subscribers.

**Notification**
```json
{
  "method": "SendInfo",
  "params": {
    "info": {"cursor": 42, "selection": [10, 20]},
    "file": {"hostId": "server-id", "project": "myproject", "file": "doc.txt"}
  }
}
```

**Flow**
1. Editor sends SendInfo notification with metadata and file
2. **If file is remote**:
   - Get `doc_id` and `site_id` from RemoteDoc
   - Create Info message with `doc_id`, sender `site_id`, and serialized value
   - Send `Msg::InfoFromClient(info)` to remote host
   - **Remote server processing**:
     - Receives info containing `doc_id`, sender `site_id`, and value
     - Broadcasts info to all subscribers of the document
     - Sends `Msg::InfoFromServer(info)` to each subscriber
     - Skips sending back to original sender
   - **Other clients receive**:
     - Receive `Msg::InfoFromServer` notification
     - Match `doc_id` to their remote documents
     - Send InfoReceived notification to their editor
3. **If file is local**:
   - Find document by PathId
   - Create Info message with `doc_id` and `site_id`
   - Broadcast `Msg::InfoFromServer(info)` to all subscribers
   - Skip sending to ourselves

**Note**: SendInfo is a notification, not a request. No response is sent back to the editor.

## Error Handling

### Error Codes

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

### Error Response Format

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

### Common Error Scenarios

1. **Remote host not connected**:
   - Occurs when trying to access files on disconnected remote
   - Server checks connection status before forwarding
   - Returns NotConnected error

2. **File not found**:
   - Local: File doesn't exist on disk
   - Remote: File not in `remote_docs` map
   - Returns IoError with descriptive message

3. **Permission denied**:
   - Remote server rejects access based on credentials
   - File system denies read/write access
   - Returns PermissionDenied error

4. **Document corruption**:
   - Engine detects inconsistent state
   - Operation sequence violation
   - Returns DocFatal, requires reopening document

5. **Invalid parameters**:
   - Missing required fields
   - Invalid file descriptors
   - Reserved project names
   - Returns BadRequest or InvalidParams

### Error Recovery

- **Connection errors**: Server implements exponential backoff for reconnection
- **Document errors**: Editor should close and reopen the document
- **File errors**: Editor should refresh file list and retry
- **Permission errors**: User must update credentials or trusted hosts
