package com.promtuz.chat.ui.activities

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Bundle
import android.os.SystemClock
import android.window.OnBackInvokedDispatcher
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts
import androidx.annotation.RequiresPermission
import androidx.appcompat.app.AppCompatActivity
import androidx.camera.core.Camera
import androidx.camera.core.ExperimentalGetImage
import androidx.camera.core.ImageAnalysis
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import com.google.common.util.concurrent.ListenableFuture
import com.google.mlkit.vision.barcode.BarcodeScanner
import com.google.mlkit.vision.barcode.BarcodeScannerOptions
import com.google.mlkit.vision.barcode.BarcodeScanning
import com.google.mlkit.vision.barcode.ZoomSuggestionOptions
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.common.InputImage
import com.promtuz.chat.presentation.viewmodel.QrScannerVM
import com.promtuz.chat.ui.screens.QrScannerScreen
import com.promtuz.chat.ui.theme.PromtuzTheme
import com.promtuz.chat.utils.extensions.then
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import org.koin.androidx.viewmodel.ext.android.viewModel
import timber.log.Timber

private const val INVALID_TIME = -1L

@ExperimentalGetImage
class QrScanner : AppCompatActivity() {
    private val viewModel: QrScannerVM by viewModel()

    lateinit var cameraProviderFuture: ListenableFuture<ProcessCameraProvider>
    lateinit var barcodeScanner: BarcodeScanner
    lateinit var camera: Camera
    var previewView: androidx.camera.view.PreviewView? = null

    companion object {
        private const val TAG = "QrScanner"
    }

    var requestPermissionLauncher: ActivityResultLauncher<String> = registerForActivityResult(
        ActivityResultContracts.RequestPermission()
    ) { viewModel.handleCameraPermissionRequest(it) }

    val expectsResult: Boolean by lazy {
        callingActivity != null
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        enableEdgeToEdge()

        setContent {
            PromtuzTheme {
                QrScannerScreen(this, viewModel)
            }
        }
    }

    @RequiresPermission(Manifest.permission.CAMERA)
    fun initScanner() {

        cameraProviderFuture = ProcessCameraProvider.getInstance(this)

        val scannerOptions =
            BarcodeScannerOptions.Builder().setBarcodeFormats(Barcode.FORMAT_QR_CODE)
                .setExecutor(ContextCompat.getMainExecutor(this)).setZoomSuggestionOptions(
                    ZoomSuggestionOptions.Builder { suggestedZoom ->
                        val control = camera.cameraControl
                        val info = camera.cameraInfo
                        val current = info.zoomState.value?.zoomRatio ?: 1f
                        val steps = 20
                        val diff = (suggestedZoom - current) / steps

                        CoroutineScope(Dispatchers.Main).launch {
                            repeat(steps) {
                                control.setZoomRatio(current + diff * (it + 1))
                                delay(16)
                            }
                        }
                        true
                    }.setMaxSupportedZoomRatio(1.5f).build()
                ).build()

        barcodeScanner = BarcodeScanning.getClient(scannerOptions)

        cameraProviderFuture.addListener({
            viewModel.setCameraProvider(cameraProviderFuture.get())
        }, ContextCompat.getMainExecutor(this))

        unfreezeCamera()
    }

    fun freezeCamera() {
        Timber.tag(TAG).d("Freezing Camera")

        // Capture the current preview frame before freezing
        try {
            previewView?.bitmap?.let { bitmap ->
                viewModel.setFrozenFrame(bitmap)
                Timber.tag(TAG).d("Captured frame: ${bitmap.width}x${bitmap.height}")
            }
        } catch (e: Exception) {
            Timber.tag(TAG).e(e, "Failed to capture frame")
        }

        viewModel.cameraProviderState.value?.unbind()
        viewModel.imageAnalysis.clearAnalyzer()
    }

    fun unfreezeCamera() {
        Timber.tag(TAG).d("Unfreezing Camera")

        viewModel.setFrozenFrame(null)
        viewModel.imageAnalysis.setAnalyzer(
            ContextCompat.getMainExecutor(this), qrAnalyzer()
        )
    }

    private var isScanning = false
    private var scanInterval = 500L // milliseconds
    private var lastScanTime: Long = INVALID_TIME // milliseconds

    /**
     * FIXME:
     *  Scanner is unaware of screen's rotation
     */
    private fun qrAnalyzer() = ImageAnalysis.Analyzer { imageProxy ->
        val now = SystemClock.uptimeMillis()

        if ((lastScanTime != INVALID_TIME && (now - lastScanTime < scanInterval)) || isScanning) {
            return@Analyzer imageProxy.close()
        }

        isScanning = true

        val inputImage = InputImage.fromMediaImage(imageProxy.image ?: return@Analyzer, 90)

        barcodeScanner.process(inputImage).addOnSuccessListener { barcodes ->
            lastScanTime = SystemClock.uptimeMillis()
            viewModel.handleScannedBarcodes(barcodes)
        }.addOnFailureListener { exception ->
            Timber.tag("QrScanner").e(exception, "Scan Fail: ")

            expectsResult.then {
                setResult(RESULT_CANCELED, Intent().putExtra("exception", exception))
            }
        }.addOnCompleteListener {
            imageProxy.close()
            isScanning = false
        }
    }

    fun handleReceiveCallback() {

    }

    /**
     * TODO:
     *  After granting permission manually from settings,
     *  UI doesn't detect change in permission.
     */
    fun checkAndInitialize() {
        if (ActivityCompat.checkSelfPermission(
                this, Manifest.permission.CAMERA
            ) == PackageManager.PERMISSION_GRANTED
        ) {
            this.initScanner()
        }
    }
}