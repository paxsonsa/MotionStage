# Understanding a session lifecycle

- Client establishes a connection to the server.
- Internally the session is created and once the connection is initialized an OpenSession Command is invoked internally.
- The server sends a Hello event to the client.
- The client sends a SessionInit command.
- The server sends a SessionStarted event to all clients.

