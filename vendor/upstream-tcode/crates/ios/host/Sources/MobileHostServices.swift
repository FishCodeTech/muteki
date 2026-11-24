import AVFoundation
import PhotosUI
import UIKit
import UniformTypeIdentifiers

private struct MobileHostServiceError: LocalizedError {
    let message: String

    init(_ message: String) {
        self.message = message
    }

    var errorDescription: String? { message }
}

@discardableResult
private func copyUTF8(
    _ value: String,
    to destination: UnsafeMutablePointer<UInt8>?,
    capacity: Int
) -> Int {
    let bytes = Array(value.utf8)
    guard let destination, capacity >= bytes.count else { return bytes.count }
    bytes.withUnsafeBufferPointer { buffer in
        if let source = buffer.baseAddress, !bytes.isEmpty {
            destination.update(from: source, count: bytes.count)
        }
    }
    return bytes.count
}

private func topPresenter() -> UIViewController? {
    var presenter = GPUIHostBridge.controller
        ?? GPUIHostBridge.view?.window?.rootViewController
    while let presented = presenter?.presentedViewController {
        presenter = presented
    }
    return presenter
}

@_cdecl("tcode_ios_host_device_name")
public func tcodeIosHostDeviceName(
    _ destination: UnsafeMutablePointer<UInt8>?,
    _ capacity: Int
) -> Int {
    copyUTF8(UIDevice.current.name, to: destination, capacity: capacity)
}

@_cdecl("tcode_ios_host_device_platform")
public func tcodeIosHostDevicePlatform(
    _ destination: UnsafeMutablePointer<UInt8>?,
    _ capacity: Int
) -> Int {
    let device = UIDevice.current
    return copyUTF8(
        "\(device.systemName) \(device.systemVersion)",
        to: destination,
        capacity: capacity
    )
}

@_cdecl("tcode_ios_host_system_locale")
public func tcodeIosHostSystemLocale(
    _ destination: UnsafeMutablePointer<UInt8>?,
    _ capacity: Int
) -> Int {
    let locale = Locale.preferredLanguages.first ?? Locale.current.identifier
    return copyUTF8(locale, to: destination, capacity: capacity)
}

@_cdecl("tcode_ios_host_start_camera_scan")
public func tcodeIosHostStartCameraScan(_ requestId: UInt64) {
    func begin() {
        guard let device = AVCaptureDevice.default(
            .builtInWideAngleCamera,
            for: .video,
            position: .back
        ) ?? AVCaptureDevice.default(for: .video) else {
            completeCamera(
                requestId,
                result: .failure(MobileHostServiceError("此设备没有可用的相机"))
            )
            return
        }
        guard let presenter = topPresenter() else {
            completeCamera(
                requestId,
                result: .failure(MobileHostServiceError("无法显示相机扫描界面"))
            )
            return
        }
        let scanner = QRScannerViewController(device: device) { result in
            completeCamera(requestId, result: result)
        }
        scanner.modalPresentationStyle = .fullScreen
        presenter.present(scanner, animated: true)
    }

    switch AVCaptureDevice.authorizationStatus(for: .video) {
    case .authorized:
        begin()
    case .notDetermined:
        AVCaptureDevice.requestAccess(for: .video) { granted in
            DispatchQueue.main.async {
                if granted {
                    begin()
                } else {
                    completeCamera(
                        requestId,
                        result: .failure(MobileHostServiceError("相机权限被拒绝"))
                    )
                }
            }
        }
    default:
        completeCamera(
            requestId,
            result: .failure(MobileHostServiceError("相机权限被拒绝"))
        )
    }
}

/// Delegates of pickers on screen, keyed by request; a PHPicker does not
/// retain its delegate.
private var imagePickers: [UInt64: ImagePickerDelegate] = [:]

@_cdecl("tcode_ios_host_pick_images")
public func tcodeIosHostPickImages(_ requestId: UInt64, _ limit: Int) {
    guard let presenter = topPresenter() else {
        finishImagePick(requestId, error: "无法显示相册")
        return
    }
    var configuration = PHPickerConfiguration(photoLibrary: .shared())
    configuration.filter = .images
    configuration.selectionLimit = max(1, limit)
    // HEIC and other formats the machine cannot decode arrive as JPEG.
    configuration.preferredAssetRepresentationMode = .compatible
    let delegate = ImagePickerDelegate(requestId: requestId)
    imagePickers[requestId] = delegate
    let picker = PHPickerViewController(configuration: configuration)
    picker.delegate = delegate
    presenter.present(picker, animated: true)
}

private func finishImagePick(_ requestId: UInt64, error: String?) {
    imagePickers[requestId] = nil
    if let error {
        withUTF8(error) { bytes, length in
            tcode_ios_image_pick_finished(requestId, bytes, length)
        }
    } else {
        tcode_ios_image_pick_finished(requestId, nil, 0)
    }
}

private final class ImagePickerDelegate: NSObject, PHPickerViewControllerDelegate {
    private let requestId: UInt64
    /// Formats delivered as they are; anything else loads as a UIImage and
    /// leaves as JPEG.
    private static let passthrough: [(UTType, String, String)] = [
        (.png, "image/png", "png"),
        (.jpeg, "image/jpeg", "jpg"),
        (.gif, "image/gif", "gif"),
        (.webP, "image/webp", "webp"),
    ]

    init(requestId: UInt64) {
        self.requestId = requestId
    }

    func picker(_ picker: PHPickerViewController, didFinishPicking results: [PHPickerResult]) {
        picker.dismiss(animated: true)
        let requestId = self.requestId
        let providers = results.map(\.itemProvider)
        // Loads complete on arbitrary queues; deliver in selection order from
        // the main queue, as every other host callback does.
        DispatchQueue.global(qos: .userInitiated).async {
            var loaded: [(String, String, Data)] = []
            var failure: String?
            for (index, provider) in providers.enumerated() {
                let stem = provider.suggestedName ?? "photo-\(index + 1)"
                switch Self.load(provider) {
                case .success(let (mime, ext, data)):
                    loaded.append(("\(stem).\(ext)", mime, data))
                case .failure(let error):
                    failure = error.localizedDescription
                }
            }
            DispatchQueue.main.async {
                for (name, mime, data) in loaded {
                    withUTF8(name) { nameBytes, nameLength in
                        withUTF8(mime) { mimeBytes, mimeLength in
                            data.withUnsafeBytes { buffer in
                                tcode_ios_image_picked(
                                    requestId,
                                    nameBytes,
                                    nameLength,
                                    mimeBytes,
                                    mimeLength,
                                    buffer.bindMemory(to: UInt8.self).baseAddress,
                                    buffer.count
                                )
                            }
                        }
                    }
                }
                finishImagePick(requestId, error: loaded.isEmpty ? failure : nil)
            }
        }
    }

    private static func load(_ provider: NSItemProvider) -> Result<(String, String, Data), Error> {
        for (type, mime, ext) in passthrough
        where provider.hasItemConformingToTypeIdentifier(type.identifier) {
            return loadData(provider, type: type).map { (mime, ext, $0) }
        }
        return loadObject(provider).flatMap { image in
            guard let data = image.jpegData(compressionQuality: 0.9) else {
                return .failure(MobileHostServiceError("无法编码所选图片"))
            }
            return .success(("image/jpeg", "jpg", data))
        }
    }

    private static func loadData(_ provider: NSItemProvider, type: UTType) -> Result<Data, Error> {
        let done = DispatchSemaphore(value: 0)
        var result: Result<Data, Error> = .failure(MobileHostServiceError("无法读取所选图片"))
        provider.loadDataRepresentation(forTypeIdentifier: type.identifier) { data, error in
            if let data {
                result = .success(data)
            } else if let error {
                result = .failure(error)
            }
            done.signal()
        }
        done.wait()
        return result
    }

    private static func loadObject(_ provider: NSItemProvider) -> Result<UIImage, Error> {
        let done = DispatchSemaphore(value: 0)
        var result: Result<UIImage, Error> = .failure(MobileHostServiceError("无法读取所选图片"))
        provider.loadObject(ofClass: UIImage.self) { object, error in
            if let image = object as? UIImage {
                result = .success(image)
            } else if let error {
                result = .failure(error)
            }
            done.signal()
        }
        done.wait()
        return result
    }
}

private func completeCamera(_ requestId: UInt64, result: Result<String, Error>) {
    switch result {
    case .success(let value):
        withUTF8(value) { valueBytes, valueLength in
            tcode_ios_camera_scan_completed(
                requestId,
                valueBytes,
                valueLength,
                nil,
                0
            )
        }
    case .failure(let error):
        withUTF8(error.localizedDescription) { errorBytes, errorLength in
            tcode_ios_camera_scan_completed(
                requestId,
                nil,
                0,
                errorBytes,
                errorLength
            )
        }
    }
}

private final class QRScannerViewController: UIViewController,
    AVCaptureMetadataOutputObjectsDelegate
{
    private let device: AVCaptureDevice
    private let completion: (Result<String, Error>) -> Void
    private let session = AVCaptureSession()
    private let previewLayer = AVCaptureVideoPreviewLayer()
    private var finished = false

    init(
        device: AVCaptureDevice,
        completion: @escaping (Result<String, Error>) -> Void
    ) {
        self.device = device
        self.completion = completion
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) {
        fatalError("QRScannerViewController must be created programmatically")
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black

        previewLayer.videoGravity = .resizeAspectFill
        previewLayer.session = session
        view.layer.addSublayer(previewLayer)

        let frame = UIView()
        frame.isUserInteractionEnabled = false
        frame.layer.cornerRadius = 24
        frame.layer.borderWidth = 3
        frame.layer.borderColor = UIColor.systemBlue.cgColor
        frame.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(frame)

        let cancel = UIButton(type: .system)
        cancel.setTitle("取消", for: .normal)
        cancel.setTitleColor(.white, for: .normal)
        cancel.titleLabel?.font = .systemFont(ofSize: 17, weight: .semibold)
        cancel.backgroundColor = UIColor.black.withAlphaComponent(0.45)
        cancel.layer.cornerRadius = 18
        cancel.addTarget(self, action: #selector(cancelScan), for: .touchUpInside)
        cancel.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(cancel)

        let label = UILabel()
        label.text = "将二维码置于取景框内"
        label.textColor = .white
        label.font = .systemFont(ofSize: 17, weight: .semibold)
        label.textAlignment = .center
        label.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(label)

        NSLayoutConstraint.activate([
            frame.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            frame.centerYAnchor.constraint(equalTo: view.centerYAnchor),
            frame.widthAnchor.constraint(equalTo: view.widthAnchor, multiplier: 0.78),
            frame.heightAnchor.constraint(equalTo: frame.widthAnchor),
            cancel.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 12),
            cancel.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 20),
            cancel.widthAnchor.constraint(equalToConstant: 72),
            cancel.heightAnchor.constraint(equalToConstant: 44),
            label.bottomAnchor.constraint(equalTo: frame.topAnchor, constant: -20),
            label.centerXAnchor.constraint(equalTo: view.centerXAnchor),
        ])

        configureSession()
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        previewLayer.frame = view.bounds
    }

    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        if session.isRunning {
            DispatchQueue.global(qos: .userInitiated).async { [session] in
                session.stopRunning()
            }
        }
    }

    private func configureSession() {
        do {
            let input = try AVCaptureDeviceInput(device: device)
            let output = AVCaptureMetadataOutput()
            session.beginConfiguration()
            guard session.canAddInput(input), session.canAddOutput(output) else {
                session.commitConfiguration()
                finish(.failure(MobileHostServiceError("无法启动二维码扫描")))
                return
            }
            session.addInput(input)
            session.addOutput(output)
            output.setMetadataObjectsDelegate(self, queue: .main)
            output.metadataObjectTypes = [.qr]
            session.commitConfiguration()
            DispatchQueue.global(qos: .userInitiated).async { [session] in
                session.startRunning()
            }
        } catch {
            finish(.failure(error))
        }
    }

    @objc private func cancelScan() {
        finish(.failure(MobileHostServiceError("已取消扫描")))
    }

    func metadataOutput(
        _ output: AVCaptureMetadataOutput,
        didOutput metadataObjects: [AVMetadataObject],
        from connection: AVCaptureConnection
    ) {
        guard let object = metadataObjects.first as? AVMetadataMachineReadableCodeObject,
              let value = object.stringValue
        else { return }
        finish(.success(value))
    }

    private func finish(_ result: Result<String, Error>) {
        guard !finished else { return }
        finished = true
        dismiss(animated: true) { [completion] in completion(result) }
    }
}

@_cdecl("tcode_ios_host_set_app_background_dark")
public func tcodeHostSetAppBackgroundDark(_ dark: UInt8) {
    GPUIHostBridge.controller?.setAppBackgroundDark(dark != 0)
}
