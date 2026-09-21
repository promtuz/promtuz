package com.promtuz.chat.ui.screens

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.provider.Settings
import android.util.Rational
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.Toast
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.Camera
import androidx.camera.core.CameraSelector
import androidx.camera.core.Preview
import androidx.camera.core.UseCaseGroup
import androidx.camera.core.ViewPort
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.CenterAlignedTopAppBar
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import androidx.core.content.ContextCompat
import androidx.core.view.doOnLayout
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.promtuz.chat.ui.components.MorphGlyph
import com.promtuz.chat.ui.components.TopBarMorphIcon
import com.promtuz.chat.R
import com.promtuz.chat.presentation.state.PermissionState
import com.promtuz.chat.presentation.viewmodel.QrScannerVM
import com.promtuz.chat.ui.text.avgSizeInStyle
import com.promtuz.chat.ui.views.QrScannerOverlayView
import org.koin.androidx.compose.koinViewModel


/**
 * Full-screen QR scanner shown as a modal with a result callback — not a nav destination or an
 * Activity. Summon it behind a `showScanner` flag; it validates the QR as a promtuz invite, hands
 * the bytes back via [onResult], and closes itself.
 */
@Composable
fun QrScannerSheet(
    onResult: (ByteArray) -> Unit,
    onDismiss: () -> Unit,
) {
    Dialog(
        onDismissRequest = onDismiss,
        properties = DialogProperties(usePlatformDefaultWidth = false),
    ) {
        val viewModel: QrScannerVM = koinViewModel()
        val context = LocalContext.current
        var camera by remember { mutableStateOf<Camera?>(null) }

        val permission by viewModel.cameraPermissionState.collectAsState()
        val cameraProvider by viewModel.cameraProviderState.collectAsState()
        val scanError by viewModel.scanError.collectAsState()
        val scanned by viewModel.scanned.collectAsState()

        val permissionLauncher = rememberLauncherForActivityResult(
            ActivityResultContracts.RequestPermission()
        ) { viewModel.handleCameraPermissionRequest(it) }

        // VM outlives this modal (shared store) — reset it so the next open is a clean session.
        DisposableEffect(Unit) { onDispose { viewModel.reset() } }

        Box(
            Modifier
                .fillMaxSize()
                .background(Color.Black)
        ) {
            when (permission) {
                PermissionState.NotRequested -> LaunchedEffect(Unit) {
                    val granted = ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) ==
                            PackageManager.PERMISSION_GRANTED
                    if (granted) viewModel.handleCameraPermissionRequest(true)
                    else permissionLauncher.launch(Manifest.permission.CAMERA)
                }

                PermissionState.Denied -> PermissionDenied()

                PermissionState.Granted -> LaunchedEffect(Unit) { viewModel.initScanner(context) }
            }

            cameraProvider?.let {
                CameraPreview(it, viewModel, Modifier.fillMaxSize()) { c -> camera = c }
            }

            LaunchedEffect(scanError) {
                scanError?.let {
                    Toast.makeText(context, it, Toast.LENGTH_SHORT).show()
                    viewModel.clearScanError()
                }
            }

            LaunchedEffect(scanned) {
                scanned?.let { onResult(it) }
            }

            QrScannerTopBar(camera, viewModel, onBack = onDismiss)
        }
    }
}


@Composable
private fun BoxScope.PermissionDenied() {
    val context = LocalContext.current
    Column(
        Modifier
            .padding(32.dp)
            .align(Alignment.Center),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(12.dp)
    ) {
        Text(
            "Camera permission denied. Enable it in Settings to scan QR",
            style = MaterialTheme.typography.titleLargeEmphasized,
            color = MaterialTheme.colorScheme.onBackground,
            textAlign = TextAlign.Center
        )
        Button({
            context.startActivity(
                Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS).apply {
                    data = Uri.fromParts("package", context.packageName, null)
                }
            )
        }) { Text("Open Settings") }
    }
}


@Composable
private fun QrScannerTopBar(camera: Camera?, viewModel: QrScannerVM, onBack: () -> Unit) {
    val textTheme = MaterialTheme.typography
    var torchEnabled by remember { mutableStateOf(false) }
    val haveCamera by viewModel.isCameraAvailable.collectAsState()

    CenterAlignedTopAppBar(
        colors = TopAppBarDefaults.topAppBarColors(containerColor = Color.Transparent),
        modifier = Modifier.background(
            Brush.verticalGradient(listOf(Color.Black.copy(alpha = 0.6f), Color.Transparent))
        ),
        navigationIcon = {
            IconButton(onBack) {
                TopBarMorphIcon(
                    MorphGlyph.Back, "Close",
                    tint = MaterialTheme.colorScheme.onSurface,
                )
            }
        },
        title = {
            Text(
                "Scan QR",
                style = avgSizeInStyle(textTheme.titleLargeEmphasized, textTheme.titleMediumEmphasized)
            )
        },
        actions = {
            if (haveCamera && camera != null) {
                IconButton({
                    torchEnabled = !torchEnabled
                    camera.cameraControl.enableTorch(torchEnabled)
                }) {
                    Icon(
                        painterResource(if (torchEnabled) R.drawable.i_flash_off else R.drawable.i_flash_on),
                        if (torchEnabled) "Turn Flash Off" else "Turn Flash On",
                        Modifier, MaterialTheme.colorScheme.onSurface
                    )
                }
            }
        }
    )
}


@Composable
private fun CameraPreview(
    cameraProvider: ProcessCameraProvider,
    viewModel: QrScannerVM,
    modifier: Modifier,
    onCamera: (Camera) -> Unit,
) {
    val lifecycleOwner = LocalLifecycleOwner.current

    AndroidView(
        factory = { context ->
            FrameLayout(context).apply {
                val previewView = PreviewView(context).apply {
                    scaleType = PreviewView.ScaleType.FILL_CENTER
                }
                val overlay = QrScannerOverlayView(context).apply {
                    layoutParams = ViewGroup.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT
                    )
                }
                addView(previewView)
                addView(overlay)
                tag = previewView
            }
        },
        update = { frameLayout ->
            val previewView = frameLayout.tag as PreviewView
            previewView.doOnLayout {
                val preview = Preview.Builder().build().also { it.surfaceProvider = previewView.surfaceProvider }
                val viewPort = ViewPort.Builder(
                    Rational(previewView.width, previewView.height), previewView.display.rotation
                ).build()
                val useCaseGroup = UseCaseGroup.Builder()
                    .addUseCase(preview)
                    .addUseCase(viewModel.imageAnalysis)
                    .setViewPort(viewPort)
                    .build()
                cameraProvider.unbindAll()
                onCamera(
                    cameraProvider.bindToLifecycle(
                        lifecycleOwner, CameraSelector.DEFAULT_BACK_CAMERA, useCaseGroup
                    )
                )
                viewModel.makeCameraAvailable()
            }
        },
        modifier = modifier
    )
}
