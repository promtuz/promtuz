package com.promtuz.chat.presentation.viewmodel

import android.app.Application
import android.content.Context
import androidx.camera.core.ImageAnalysis
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.google.mlkit.vision.barcode.common.Barcode
import com.promtuz.chat.domain.model.Identity
import com.promtuz.chat.presentation.state.PermissionState
import com.promtuz.core.API
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import timber.log.Timber

class QrScannerVM(
    private val application: Application, private val api: API
) : ViewModel() {
    companion object {
        @Volatile
        private var instance: QrScannerVM? = null

        @JvmStatic
        fun onIdentityQrScanned(name: String) {
            instance?.onIdentityQrDetected(name)
        }

        internal fun setInstance(vm: QrScannerVM) {
            instance = vm
        }
    }

    private val context: Context get() = application.applicationContext
    private val log = Timber.tag("QrScannerVM")

    var imageAnalysis =
        ImageAnalysis.Builder().setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
            .build()

    private val _isCameraAvailable = MutableStateFlow(false)
    val isCameraAvailable = _isCameraAvailable.asStateFlow()

    private val _cameraPermissionState = MutableStateFlow(PermissionState.NotRequested)
    val cameraPermissionState = _cameraPermissionState.asStateFlow()

    private val _cameraProviderState = MutableStateFlow<ProcessCameraProvider?>(null)
    val cameraProviderState = _cameraProviderState.asStateFlow()

    private val _selectedIdentity = MutableStateFlow<Identity?>(null)
    val selectedIdentity = _selectedIdentity.asStateFlow()

    private val _identities = MutableStateFlow<List<Identity>>(emptyList())
    val identities = _identities.asStateFlow()

    private val _isProcessingIdentity = MutableStateFlow(false)
    val isProcessingIdentity = _isProcessingIdentity.asStateFlow()

    private val _processingIdentityName = MutableStateFlow<String?>(null)
    val processingIdentityName = _processingIdentityName.asStateFlow()

    private val _frozenFrameBitmap = MutableStateFlow<android.graphics.Bitmap?>(null)
    val frozenFrameBitmap = _frozenFrameBitmap.asStateFlow()

    private val _backPressedOnce = MutableStateFlow(false)
    val backPressedOnce = _backPressedOnce.asStateFlow()

    init {
        setInstance(this)
    }

    fun setCameraProvider(provider: ProcessCameraProvider) {
        _cameraProviderState.value = provider
    }

    fun handleCameraPermissionRequest(isGranted: Boolean) {
        if (isGranted) {
            _cameraPermissionState.value = PermissionState.Granted
        } else {
            _cameraPermissionState.value = PermissionState.Denied
        }
    }

    fun makeCameraAvailable() {
        _isCameraAvailable.value = true
    }

    fun handleScannedBarcodes(barcodes: List<Barcode>) = viewModelScope.launch {
        _identities.value = (barcodes.mapNotNull { barcode ->
            barcode.rawBytes?.let { bytes ->
                API.parseQRBytes(bytes)
                null
            } ?: return@mapNotNull null
        }).distinctBy { it }
    }

    fun dismissIdentity() {
        _selectedIdentity.value = null
        _identities.update { emptyList() }
    }

    fun saveUserIdentity(userIdentity: Identity) {
        _selectedIdentity.value = userIdentity
    }

    fun onIdentityQrDetected(name: String) {
        log.d("Identity QR detected: $name")
        _isProcessingIdentity.value = true
        _processingIdentityName.value = name
        _backPressedOnce.value = false
    }

    fun setFrozenFrame(bitmap: android.graphics.Bitmap?) {
        _frozenFrameBitmap.value = bitmap
    }

    fun onBackPressedDuringProcessing() {
        println("SYSTEM BACK PRESS -  ${_backPressedOnce.value}")
        if (_backPressedOnce.value) {
            // Second press - actually cancel
            dismissProcessing()
        } else {
            // First press - show warning
            _backPressedOnce.value = true
        }
    }

    fun dismissProcessing() {
        _isProcessingIdentity.value = false
        _processingIdentityName.value = null
        _frozenFrameBitmap.value = null
        _backPressedOnce.value = false
        // TODO: Notify peer if connected (for future implementation)
    }
}