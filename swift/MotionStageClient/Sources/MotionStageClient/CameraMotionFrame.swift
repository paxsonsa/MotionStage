import simd

/// Swift-native camera motion frame. Uses `sendBatch` internally.
/// Replaces the FFI-coupled `MotionFrame` + `sendMotionFrame` pair (5.4).
public struct CameraMotionFrame: Sendable {
    public var position: SIMD3<Float>
    public var rotation: simd_quatf
    public var velocity: SIMD3<Float>
    public var focalLength: Float
    public var focusDistance: Float
    public var aperture: Float
    public var fieldMask: FieldMask

    public init(
        position: SIMD3<Float> = .zero,
        rotation: simd_quatf = simd_quatf(ix: 0, iy: 0, iz: 0, r: 1),
        velocity: SIMD3<Float> = .zero,
        focalLength: Float = 50.0,
        focusDistance: Float = 1.0,
        aperture: Float = 2.8,
        fieldMask: FieldMask = .allMotion
    ) {
        self.position = position
        self.rotation = rotation
        self.velocity = velocity
        self.focalLength = focalLength
        self.focusDistance = focusDistance
        self.aperture = aperture
        self.fieldMask = fieldMask
    }

    /// Send this frame via `client.sendBatch(_:)`.
    public func send(via client: MotionStageClient) throws {
        var updates: [(key: AttributeKey, values: [Float])] = []
        updates.reserveCapacity(6)

        if fieldMask.contains(.position) {
            updates.append((Attribute.Motion.position, [position.x, position.y, position.z]))
        }
        if fieldMask.contains(.rotation) {
            updates.append((Attribute.Motion.rotation,
                            [rotation.imag.x, rotation.imag.y, rotation.imag.z, rotation.real]))
        }
        if fieldMask.contains(.velocity) {
            updates.append((Attribute.Motion.velocity, [velocity.x, velocity.y, velocity.z]))
        }
        if fieldMask.contains(.focalLength) {
            updates.append((Attribute.Camera.focalLength, [focalLength]))
        }
        if fieldMask.contains(.focusDistance) {
            updates.append((Attribute.Camera.focusDistance, [focusDistance]))
        }
        if fieldMask.contains(.aperture) {
            updates.append((Attribute.Camera.aperture, [aperture]))
        }

        try client.sendBatch(updates)
    }
}
