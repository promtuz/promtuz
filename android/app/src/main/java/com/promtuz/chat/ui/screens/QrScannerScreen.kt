@file:androidx.annotation.OptIn(ExperimentalGetImage::class)

package com.promtuz.chat.ui.screens

import android.Manifest
import android.content.Intent
import android.net.Uri
import android.provider.Settings
import android.util.Rational
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.Toast
import androidx.camera.core.CameraSelector
import androidx.camera.core.ExperimentalGetImage
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
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.view.doOnLayout
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.promtuz.chat.R
import com.promtuz.chat.presentation.state.PermissionState
import com.promtuz.chat.presentation.viewmodel.QrScannerVM
import com.promtuz.chat.LauncherActivity
import com.promtuz.chat.ui.activities.QrScanner
import com.promtuz.chat.utils.InviteLink
import com.promtuz.chat.ui.components.GoBackButton
import com.promtuz.chat.ui.text.avgSizeInStyle
import com.promtuz.chat.ui.views.QrScannerOverlayView


@Composable
fun QrScannerScreen(
    activity: QrScanner,
    viewModel: QrScannerVM
) {
    Box(
        Modifier.fillMaxSize()
    ) {
        val cameraProvider by viewModel.cameraProviderState.collectAsState()
        val scanError by viewModel.scanError.collectAsState()
        val scanned by viewModel.scanned.collectAsState()

        PermissionRequester(activity, viewModel)

        cameraProvider?.let {
            CameraPreview(
                activity, it, Modifier
                    .fillMaxSize(), viewModel
            )
        }

        LaunchedEffect(scanError) {
            scanError?.let {
                Toast.makeText(activity, it, Toast.LENGTH_SHORT).show()
                viewModel.clearScanError()
            }
        }

        LaunchedEffect(scanned) {
            scanned?.let { bytes ->
                // Hand the invite to the same confirm sheet the link flow uses:
                // App consumes EXTRA_INVITE and raises InviteBottomSheet.
                activity.startActivity(
                    Intent(activity, LauncherActivity::class.java).apply {
                        putExtra(InviteLink.EXTRA_INVITE, bytes)
                        addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP)
                    }
                )
                activity.finish()
            }
        }

        QrScannerTopBar(activity, viewModel)
    }
}


@Composable
private fun BoxScope.PermissionRequester(activity: QrScanner, viewModel: QrScannerVM) {
    val cameraPermission by viewModel.cameraPermissionState.collectAsState()

    when (cameraPermission) {
        PermissionState.NotRequested -> {
            activity.requestPermissionLauncher.launch(Manifest.permission.CAMERA)
        }

        PermissionState.Denied -> {
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
                    activity.startActivity(
                        Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS).apply {
                            setData(Uri.fromParts("package", activity.packageName, null))
                        }
                    )
                }) {
                    Text("Open Settings")
                }
            }
        }

        PermissionState.Granted -> {
            activity.checkAndInitialize()
        }
    }
}

@Composable
private fun QrScannerTopBar(
    activity: QrScanner,
    viewModel: QrScannerVM
) {
    val textTheme = MaterialTheme.typography
    var torchEnabled by remember { mutableStateOf(false) }
    val haveCamera by viewModel.isCameraAvailable.collectAsState()

    CenterAlignedTopAppBar(
        colors = TopAppBarDefaults.topAppBarColors(containerColor = Color.Transparent),
        modifier = Modifier.background(
            Brush.verticalGradient(
                listOf(
                    Color.Black.copy(alpha = 0.6f),
                    Color.Transparent
                )
            )
        ),
        navigationIcon = { GoBackButton() }, title = {
            Text(
                "Scan QR", style = avgSizeInStyle(
                    textTheme.titleLargeEmphasized, textTheme.titleMediumEmphasized
                )
            )
        },
        actions = {
            if (haveCamera) {
                IconButton({
                    torchEnabled = !torchEnabled
                    activity.camera.cameraControl.enableTorch(torchEnabled)
                }) {
                    Icon(
                        painter = if (torchEnabled) painterResource(R.drawable.i_flash_off) else painterResource(
                            R.drawable.i_flash_on
                        ),
                        if (torchEnabled) "Turn Flash Off" else "Turn Flash On",
                        Modifier,
                        MaterialTheme.colorScheme.onSurface
                    )
                }
            }
        }

    )
}

@Composable
private fun CameraPreview(
    activity: QrScanner,
    cameraProvider: ProcessCameraProvider,
    modifier: Modifier,
    viewModel: QrScannerVM
) {
    val lifecycleOwner = LocalLifecycleOwner.current

    AndroidView(
        factory = { context ->
            FrameLayout(context).apply {
                val previewView = PreviewView(context).apply {
                    scaleType = PreviewView.ScaleType.FILL_CENTER
                }

                val previewOverlay = QrScannerOverlayView(context).apply {
                    layoutParams = ViewGroup.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT,
                        ViewGroup.LayoutParams.MATCH_PARENT
                    )
                }

                addView(previewView)
                addView(previewOverlay)

                // Store reference for bitmap capture
                activity.previewView = previewView
                tag = previewView
            }
        }, update = { frameLayout ->
            val previewView = frameLayout.tag as PreviewView

            previewView.doOnLayout {
                val preview = Preview.Builder().build()
                val cameraSelector = CameraSelector.DEFAULT_BACK_CAMERA
                preview.surfaceProvider = previewView.surfaceProvider

                val viewPort = ViewPort.Builder(
                    Rational(previewView.width, previewView.height), previewView.display.rotation
                ).build()
                viewPort.aspectRatio

                val useCaseGroup =
                    UseCaseGroup.Builder().addUseCase(preview).addUseCase(viewModel.imageAnalysis)
                        .setViewPort(viewPort).build()

                cameraProvider.unbindAll()
                activity.camera =
                    cameraProvider.bindToLifecycle(lifecycleOwner, cameraSelector, useCaseGroup)

                viewModel.makeCameraAvailable()
            }
        }, modifier = modifier
    )
}
