import Foundation
import Security

private let labelPrefix = "tools-mcp-device:"

private func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

guard CommandLine.arguments.count == 4, CommandLine.arguments[1] == "generate" else {
    fail("usage: tools-mcp-keygen generate <keychain-label> <trusted-executable>")
}

let label = CommandLine.arguments[2]
let trustedExecutable = CommandLine.arguments[3]
guard label.hasPrefix(labelPrefix), label.utf8.count <= 256 else {
    fail("invalid tools-mcp Keychain label")
}
guard trustedExecutable.hasPrefix("/") else {
    fail("trusted executable path must be absolute")
}

let existingQuery: [CFString: Any] = [
    kSecClass: kSecClassKey,
    kSecAttrKeyClass: kSecAttrKeyClassPrivate,
    kSecAttrLabel: label,
    kSecMatchLimit: kSecMatchLimitOne,
]
let existingStatus = SecItemCopyMatching(existingQuery as CFDictionary, nil)
guard existingStatus == errSecItemNotFound else {
    fail(existingStatus == errSecSuccess ? "device key already exists" : "Keychain lookup failed: \(existingStatus)")
}

func trustedApplication(_ path: String) -> SecTrustedApplication {
    var application: SecTrustedApplication?
    let status = SecTrustedApplicationCreateFromPath(path, &application)
    guard status == errSecSuccess, let application else {
        fail("create trusted application for \(path): \(status)")
    }
    return application
}

let trustedApplications = [
    trustedApplication(CommandLine.arguments[0]),
    trustedApplication(trustedExecutable),
] as CFArray
var access: SecAccess?
let accessStatus = SecAccessCreate(label as CFString, trustedApplications, &access)
guard accessStatus == errSecSuccess, let access else {
    fail("create Keychain access policy: \(accessStatus)")
}

let privateAttributes: [CFString: Any] = [
    kSecAttrIsPermanent: true,
    kSecAttrLabel: label,
    kSecAttrAccess: access,
]
let attributes: [CFString: Any] = [
    kSecAttrKeyType: kSecAttrKeyTypeECSECPrimeRandom,
    kSecAttrKeySizeInBits: 256,
    kSecAttrIsExtractable: false,
    kSecAttrIsSensitive: true,
    kSecPrivateKeyAttrs: privateAttributes,
]
var creationError: Unmanaged<CFError>?
guard let key = SecKeyCreateRandomKey(attributes as CFDictionary, &creationError) else {
    let detail = creationError?.takeRetainedValue().localizedDescription ?? "unknown error"
    fail("non-exportable Keychain key generation failed: \(detail)")
}

var exportError: Unmanaged<CFError>?
guard SecKeyCopyExternalRepresentation(key, &exportError) == nil else {
    SecItemDelete([kSecValueRef: key] as CFDictionary)
    fail("Keychain returned an exportable private key")
}
