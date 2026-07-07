import Foundation
import MotionStageSwiftFFI

// MARK: - Operator-plane operations (protocol 2.1)
//
// These ops are request/reply on the control stream. A typed server
// rejection (e.g. a permission failure) throws
// `MotionStageError.operationRejected(code:reason:)` and is guaranteed to
// have mutated nothing; transport failures throw the usual
// `.notConnected`/`.transportError` cases. Successful mutations additionally
// echo on `stateEvents` (with `originSession` == this client's session).

extension MotionStageClient {
    /// Create a mapping. `sourceDevice` nil = this session's own device;
    /// `targetScene` nil = the active scene; `componentMask` nil = all
    /// components. Non-operator sessions may only create mappings sourced
    /// from their own device. Returns the server's post-default-resolution
    /// mapping summary.
    @discardableResult
    public func createMapping(
        sourceOutput: String,
        targetObject: UUID,
        targetAttribute: String,
        sourceDevice: UUID? = nil,
        targetScene: UUID? = nil,
        componentMask: [Int]? = nil
    ) async throws -> MappingSummary {
        let rawClient = self.rawClient
        return try await Task.detached(priority: .userInitiated) {
            var outJson: UnsafeMutablePointer<CChar>?
            let mask = componentMask.map { $0.map { UInt32($0) } }
            let status = withOptionalCString(sourceDevice?.uuidString) { sourceDevicePtr in
                sourceOutput.withCString { sourceOutputPtr in
                    withOptionalCString(targetScene?.uuidString) { targetScenePtr in
                        targetObject.uuidString.withCString { targetObjectPtr in
                            targetAttribute.withCString { targetAttributePtr in
                                withOptionalUInt32Buffer(mask) { maskPtr, maskLen in
                                    motionstage_swift_client_create_mapping(
                                        rawClient,
                                        sourceDevicePtr,
                                        sourceOutputPtr,
                                        targetScenePtr,
                                        targetObjectPtr,
                                        targetAttributePtr,
                                        maskPtr,
                                        maskLen,
                                        &outJson
                                    )
                                }
                            }
                        }
                    }
                }
            }
            let json = try wireResultJSON(status, outJson, rawClient, op: "create mapping")
            return try decodeWireResult(MappingSummary.self, from: json)
        }.value
    }

    /// Replace a mapping's full definition (all fields are required, matching
    /// `createMapping` semantics — this is not a partial patch). Non-operator
    /// sessions may only update mappings whose current and requested source
    /// device is their own. Returns the updated summary.
    @discardableResult
    public func updateMapping(
        mappingID: UUID,
        sourceOutput: String,
        targetObject: UUID,
        targetAttribute: String,
        sourceDevice: UUID? = nil,
        targetScene: UUID? = nil,
        componentMask: [Int]? = nil
    ) async throws -> MappingSummary {
        let rawClient = self.rawClient
        return try await Task.detached(priority: .userInitiated) {
            var outJson: UnsafeMutablePointer<CChar>?
            let mask = componentMask.map { $0.map { UInt32($0) } }
            let status = mappingID.uuidString.withCString { mappingIDPtr in
                withOptionalCString(sourceDevice?.uuidString) { sourceDevicePtr in
                    sourceOutput.withCString { sourceOutputPtr in
                        withOptionalCString(targetScene?.uuidString) { targetScenePtr in
                            targetObject.uuidString.withCString { targetObjectPtr in
                                targetAttribute.withCString { targetAttributePtr in
                                    withOptionalUInt32Buffer(mask) { maskPtr, maskLen in
                                        motionstage_swift_client_update_mapping(
                                            rawClient,
                                            mappingIDPtr,
                                            sourceDevicePtr,
                                            sourceOutputPtr,
                                            targetScenePtr,
                                            targetObjectPtr,
                                            targetAttributePtr,
                                            maskPtr,
                                            maskLen,
                                            &outJson
                                        )
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let json = try wireResultJSON(status, outJson, rawClient, op: "update mapping")
            return try decodeWireResult(MappingSummary.self, from: json)
        }.value
    }

    /// Remove a mapping (own source device, or Operator role for any).
    public func removeMapping(mappingID: UUID) async throws {
        let rawClient = self.rawClient
        try await Task.detached(priority: .userInitiated) {
            var outJson: UnsafeMutablePointer<CChar>?
            let status = mappingID.uuidString.withCString { mappingIDPtr in
                motionstage_swift_client_remove_mapping(rawClient, mappingIDPtr, &outJson)
            }
            let json = try wireResultJSON(status, outJson, rawClient, op: "remove mapping")
            try decodeWireAck(from: json)
        }.value
    }

    /// Lock or unlock a mapping (own source device, or Operator role for any).
    public func setMappingLock(mappingID: UUID, lock: Bool) async throws {
        let rawClient = self.rawClient
        try await Task.detached(priority: .userInitiated) {
            var outJson: UnsafeMutablePointer<CChar>?
            let status = mappingID.uuidString.withCString { mappingIDPtr in
                motionstage_swift_client_set_mapping_lock(
                    rawClient, mappingIDPtr, lock ? 1 : 0, &outJson
                )
            }
            let json = try wireResultJSON(status, outJson, rawClient, op: "set mapping lock")
            try decodeWireAck(from: json)
        }.value
    }

    /// Start recording a take (Operator role required). The server assigns
    /// the take identity and recording path; the returned UUID is the take id.
    @discardableResult
    public func startTake() async throws -> UUID {
        let rawClient = self.rawClient
        return try await Task.detached(priority: .userInitiated) {
            var outJson: UnsafeMutablePointer<CChar>?
            let status = motionstage_swift_client_start_take(rawClient, &outJson)
            let json = try wireResultJSON(status, outJson, rawClient, op: "start take")
            let takeIDString = try decodeWireResult(String.self, from: json)
            guard let takeID = UUID(uuidString: takeIDString) else {
                throw MotionStageError.protocolError("start take returned invalid id \(takeIDString)")
            }
            return takeID
        }.value
    }

    /// Stop the active recording and register the take (Operator role
    /// required). Returns the registered take-catalog entry.
    @discardableResult
    public func stopTake() async throws -> TakeInfo {
        let rawClient = self.rawClient
        return try await Task.detached(priority: .userInitiated) {
            var outJson: UnsafeMutablePointer<CChar>?
            let status = motionstage_swift_client_stop_take(rawClient, &outJson)
            let json = try wireResultJSON(status, outJson, rawClient, op: "stop take")
            return try decodeWireResult(TakeInfo.self, from: json)
        }.value
    }

    /// Request an on-demand full world snapshot (scenes, mappings, mode,
    /// sessions, takes, playback). Use this to populate target pickers
    /// without waiting for the next unsolicited snapshot on `stateEvents`.
    public func sceneSnapshot() async throws -> SceneSnapshot {
        let rawClient = self.rawClient
        return try await Task.detached(priority: .userInitiated) {
            var outJson: UnsafeMutablePointer<CChar>?
            let status = motionstage_swift_client_get_scene_snapshot(rawClient, &outJson)
            let json = try wireResultJSON(status, outJson, rawClient, op: "scene snapshot")
            do {
                return try JSONDecoder().decode(SceneSnapshot.self, from: Data(json.utf8))
            } catch {
                throw MotionStageError.protocolError(
                    "failed to decode scene snapshot JSON: \(error)"
                )
            }
        }.value
    }
}

// MARK: - Wire result decoding helpers

private struct WireErrorPayload: Decodable {
    let code: String
    let reason: String
}

/// Check the FFI status and take ownership of the result JSON.
private func wireResultJSON(
    _ status: Int32,
    _ outJson: UnsafeMutablePointer<CChar>?,
    _ rawClient: UnsafeMutableRawPointer,
    op: String
) throws -> String {
    guard status == MOTIONSTAGE_SWIFT_STATUS_OK else {
        // Non-OK never populates outJson; the detail lives in last_error.
        let msg = takeRustString(motionstage_swift_client_last_error(rawClient))
            ?? "\(op) failed with status \(status)"
        throw makeError(status: status, message: msg)
    }
    guard let json = takeRustString(outJson) else {
        throw MotionStageError.internalError("\(op) returned no result payload")
    }
    return json
}

/// Decode a `{"ok":<T>}` / `{"err":{"code","reason"}}` envelope, throwing
/// `MotionStageError.operationRejected` for typed wire errors.
private func decodeWireResult<T: Decodable>(_ type: T.Type, from json: String) throws -> T {
    struct Envelope<Value: Decodable>: Decodable {
        let ok: Value?
        let err: WireErrorPayload?
    }
    let envelope: Envelope<T>
    do {
        envelope = try JSONDecoder().decode(Envelope<T>.self, from: Data(json.utf8))
    } catch {
        throw MotionStageError.protocolError("failed to decode wire result JSON: \(error)")
    }
    if let err = envelope.err {
        throw MotionStageError.operationRejected(
            code: RejectCode(wireName: err.code), reason: err.reason
        )
    }
    guard let ok = envelope.ok else {
        throw MotionStageError.protocolError("wire result JSON has neither ok nor err: \(json)")
    }
    return ok
}

/// Decode a unit `{"ok":null}` / `{"err":{...}}` envelope.
private func decodeWireAck(from json: String) throws {
    struct Envelope: Decodable {
        let err: WireErrorPayload?
    }
    let envelope: Envelope
    do {
        envelope = try JSONDecoder().decode(Envelope.self, from: Data(json.utf8))
    } catch {
        throw MotionStageError.protocolError("failed to decode wire result JSON: \(error)")
    }
    if let err = envelope.err {
        throw MotionStageError.operationRejected(
            code: RejectCode(wireName: err.code), reason: err.reason
        )
    }
}

private func withOptionalUInt32Buffer<T>(
    _ values: [UInt32]?,
    _ body: (UnsafePointer<UInt32>?, UInt32) throws -> T
) rethrows -> T {
    guard let values else { return try body(nil, 0) }
    return try values.withUnsafeBufferPointer { buffer in
        try body(buffer.baseAddress, UInt32(buffer.count))
    }
}
