/// Typed attribute namespace for MotionStage (5.1).

public enum AttributeKind: Sendable {
    case float32
    case vec2f
    case vec3f
    case vec4f
    case quatf
    case mat4f

    /// Number of float components for this kind.
    public var componentCount: Int {
        switch self {
        case .float32: return 1
        case .vec2f:   return 2
        case .vec3f:   return 3
        case .vec4f:   return 4
        case .quatf:   return 4
        case .mat4f:   return 16
        }
    }

    /// C ordinal matching the Rust `AttributeKind` enum discriminant.
    public var cOrdinal: UInt32 {
        switch self {
        case .float32: return 2
        case .vec2f:   return 4
        case .vec3f:   return 5
        case .vec4f:   return 6
        case .quatf:   return 7
        case .mat4f:   return 8
        }
    }
}

/// A typed handle to an attribute path.
public struct AttributeKey: Sendable, Hashable, CustomStringConvertible {
    public let path: String
    public let kind: AttributeKind

    public init(_ path: String, type kind: AttributeKind) {
        self.path = path
        self.kind = kind
    }

    public var description: String { path }
}

/// Canonical attribute paths used by MotionStage.
public enum Attribute {
    public enum Motion {
        public static let position = AttributeKey("motion.position", type: .vec3f)
        public static let rotation = AttributeKey("motion.rotation", type: .quatf)
        public static let velocity = AttributeKey("motion.velocity", type: .vec3f)
    }

    public enum Camera {
        public static let focalLength   = AttributeKey("camera.focal_length",   type: .float32)
        public static let focusDistance = AttributeKey("camera.focus_distance", type: .float32)
        public static let aperture      = AttributeKey("camera.aperture",       type: .float32)
    }
}
