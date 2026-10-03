@file:androidx.annotation.OptIn(ExperimentalGetImage::class)

package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.content.Context
import androidx.camera.core.ExperimentalGetImage
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.core.content.ContextCompat
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.google.mlkit.vision.barcode.BarcodeScanner
import com.google.mlkit.vision.barcode.BarcodeScannerOptions
import com.google.mlkit.vision.barcode.BarcodeScanning
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.common.InputImage
import com.promtuz.chat.presentation.state.PermissionState
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import timber.log.Timber
import uniffi.core.CoreException

class QrScannerVM(
    private val application: Application
) : ViewModel() {
    private val log = Timber.tag("QrScannerVM")

    var imageAnalysis = newAnalysis()
        private set

    private var barcodeScanner: BarcodeScanner? = null

    private val _isCameraAvailable = MutableStateFlow(false)
    val isCameraAvailable = _isCameraAvailable.asStateFlow()

    private val _cameraPermissionState = MutableStateFlow(PermissionState.NotRequested)
    val cameraPermissionState = _cameraPermissionState.asStateFlow()

    private val _cameraProviderState = MutableStateFlow<ProcessCameraProvider?>(null)
    val cameraProviderState = _cameraProviderState.asStateFlow()

    private val _scanError = MutableStateFlow<String?>(null)
    val scanError = _scanError.asStateFlow()

    private val _scanned = MutableStateFlow<ByteArray?>(null)
    val scanned = _scanned.asStateFlow()

    // ML Kit delivers frames serially, so a plain flag guards against re-firing mid-validation.
    @Volatile
    private var processing = false

    private fun newAnalysis() =
        ImageAnalysis.Builder().setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST).build()

    fun initScanner(context: Context) {
        if (_cameraProviderState.value != null) return
        barcodeScanner = BarcodeScanning.getClient(
            BarcodeScannerOptions.Builder().setBarcodeFormats(Barcode.FORMAT_QR_CODE).build()
        )
        val future = ProcessCameraProvider.getInstance(context)
        future.addListener({
            _cameraProviderState.value = future.get()
            imageAnalysis.setAnalyzer(ContextCompat.getMainExecutor(context)) { analyze(it) }
        }, ContextCompat.getMainExecutor(context))
    }

    private fun analyze(imageProxy: ImageProxy) {
        val scanner = barcodeScanner
        val media = imageProxy.image
        if (scanner == null || media == null) {
            imageProxy.close()
            return
        }
        val input = InputImage.fromMediaImage(media, imageProxy.imageInfo.rotationDegrees)
        scanner.process(input)
            .addOnSuccessListener { handleScannedBarcodes(it) }
            .addOnFailureListener { log.e(it, "scan failed") }
            .addOnCompleteListener { imageProxy.close() }
    }

    /** The VM outlives the sheet, so the next open needs a clean session. */
    fun reset() {
        _cameraProviderState.value?.unbindAll()
        imageAnalysis.clearAnalyzer()
        barcodeScanner?.close()
        barcodeScanner = null
        imageAnalysis = newAnalysis()
        _cameraProviderState.value = null
        _isCameraAvailable.value = false
        _cameraPermissionState.value = PermissionState.NotRequested
        _scanError.value = null
        _scanned.value = null
        processing = false
    }

    fun clearScanError() {
        _scanError.value = null
    }

    fun handleCameraPermissionRequest(isGranted: Boolean) {
        _cameraPermissionState.value = if (isGranted) PermissionState.Granted else PermissionState.Denied
    }

    fun makeCameraAvailable() {
        _isCameraAvailable.value = true
    }

    fun handleScannedBarcodes(barcodes: List<Barcode>) {
        // One capture per session; a QR that is not an invite re-arms the scanner.
        if (processing || _scanned.value != null) return
        val bytes = barcodes.firstNotNullOfOrNull { it.rawBytes } ?: return
        processing = true
        viewModelScope.launch {
            try {
                CoreBridge.previewInvite(bytes) // throws if it isn't an invite
                imageAnalysis.clearAnalyzer()
                _scanned.value = bytes
            } catch (e: CoreException) {
                log.e(e, "not a promtuz invite")
                _scanError.value = "Not a Promtuz invite"
            } finally {
                processing = false
            }
        }
    }
}
