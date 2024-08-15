# CineMotion: Device Motion Streaming Framework
In the age of AR/VR, Virtual Production, and Computer Graphics, translating motion from the real world into the computer world is becoming a great tool for creative workflows. 
Unfortunately, existing motion capture systems are pricey, integrations to existing software you use may no exist and you may want to transmit motion from multiple devices and systems.

CineMotion is a framework/protocol that:
1) Helps embed motion streaming from devices in DCC application more easily.
2) A protocol allowing for multiple devices to stream motion data together.
3) Help integrators by avoiding worrying about the networking connectivity integration.
4) Defines a network transport so new devices can easily integrate.

# Benefits
- Most 'motion streaming' integrations are application specific (e.g. just for Unreal, Maya etc). CineMotion is DCC agnostic.
- No device/Application lock in: Many existing motion streaming tools require you to pay for an app or some hardware. CineMotion does not, it allows you to integrate your own device/software to stream data.
- Simple API for implementing python integrations (the norm in VFX/Games tools) or use the rust, c++, or swift bindings.


Design:
- CineMotion Core: This is the core engine and its systems. It uses a ECS model for representing all objects in the system.
- CineMotion Server: This is the main server which clients connect to. 
- CineMotion Renderer: This is the main renderer for the system which works with the integration layers to get new render data.

## CineMotion Core
The main engine is a ECS based design that is abstracted away from the client and connect devices. 

The main loop of the engine is processing all motion data in realtime and apply received commands as they come in.
However, the engine and scene state is captured and broadcast at a fix frame rate, which is set on the engine settings
itself. What this means is when the engine 'ticks' whatever the latest state is what is broadcast. If multiple commands
are received the latest takes precedence unless a interpolation/filter is set for an object attribute.

## CineMotion Server
The server abstract away the connectivity and communication from the main core. In fact, whether it is a network 
client or the host client DCC integration, the communication with the engine and renderer is down through this layer.
This also for the communication protocol to be agnostic to the command protocol. All command are serialized as protobufs
regardless of their transport protocol. This is to all for multiple implementations for client apis.

## CineMotion Renderer
CineMotion does not have a renderer in the traditional sense of a game engine. Instead, the renderer is really just a
video stream broadcaster that works with the DCC integration to capture the current frame, after updates have been applied,
and transmit it to eligble client devices.

## DCC Integration 
The DCC integration is fairly straight forward. If your integration will be python based you can use the python bindings 
the are supplied. For other language runtimes like C/C++, Swift, or NodeJS you can implement the communication protocol using 
the unix socket configuration and use the protobufs in this repo. The protocol for transport contains a simple header followed by
the protobuf bytes message. The server will not send ACKs only errors and state updates. 

## Commands and Events

## Initial Client Setup
- Connect to Server
- Server will send initial message with given client ID
- Client needs to acknowledge the initial message with device registration.
- Once registered the client will receive state updates

## Setup a Objects
- New Session, engine builds default root object (cannot not be removed)
- Devices can generate new objects.

There are a number of commands and events that are apart of the protocol for interacting with the server. This is an overview of them.

TrDO: Build out debugger
  - Add ability to read into JSON Array for Message sending.
  - Add ability to add object
  - Add ability to send a sample
  - Add Update Commands
TODO: Swift Protobuf build out: Start a CLI Project and start building out Swift Code with Protobufs.
TODO: Service Discovery
TODO: Prototype Blender Integration

### Server Actors
- ConnectionActor
- EngineActor
- ServerActor

### Controller/Device Commands
- RegisterDevice(device) -> id
- UpdateDevice(device)
- RemoveDevice(id) -> id 
- Sample(id, Vec<samples>)
- Reset(id)

### Scene Commands
- ResetScene
- CreateScene
- UpdateScene

### Object Commands
- CreateObject -> id
- RemoveObject -> id|null
- UpdateObject -> id|null

### Take Commands
- NewTake
- StartRecordingTake
- StopRecordingTake
- DeleteTake
- LoadTake

### Playback
- StartPlayback
- StopPlayback
- ResetPlayback
- StepForwardPlayback
- StepBackPlayback
- SeekPlayback

### Render Stream Commands
- RenderStartStream
- RenderStopStream
- RenderUpdateSettings

### Engine Commands
- ChangeMode
- UpdateSettings
