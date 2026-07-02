@file:androidx.annotation.OptIn(ExperimentalGetImage::class)

package com.promtuz.chat.ui.screens

import android.Manifest
import android.content.Intent
import android.net.Uri
import android.provider.Settings
import android.util.Rational
import android.view.ViewGroup
import android.widget.FrameLayout
import androidx.camera.core.CameraSelector
import androidx.camera.core.ExperimentalGetImage
import androidx.camera.core.Preview
import androidx.camera.core.UseCaseGroup
import androidx.camera.core.ViewPort
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.background
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CenterAlignedTopAppBar
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LoadingIndicator
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.BiasAlignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import androidx.activity.compose.BackHandler
import androidx.core.view.doOnLayout
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.promtuz.chat.R
import com.promtuz.chat.domain.model.Identity
import com.promtuz.chat.presentation.state.PermissionState
import com.promtuz.chat.presentation.viewmodel.QrScannerVM
import com.promtuz.chat.ui.activities.QrScanner
import com.promtuz.chat.ui.components.GoBackButton
import com.promtuz.chat.ui.text.avgSizeInStyle
import com.promtuz.chat.ui.views.QrScannerOverlayView


@Composable
fun QrScannerScreen(
    activity: QrScanner,
    viewModel: QrScannerVM
) {
    val selectedIdentity by viewModel.selectedIdentity.collectAsState()
    val isProcessingIdentity by viewModel.isProcessingIdentity.collectAsState()
    val processingIdentityName by viewModel.processingIdentityName.collectAsState()
    val frozenFrameBitmap by viewModel.frozenFrameBitmap.collectAsState()
    val backPressedOnce by viewModel.backPressedOnce.collectAsState()

    Box(
        Modifier.fillMaxSize()
    ) {
        val cameraProvider by viewModel.cameraProviderState.collectAsState()
        val identities by viewModel.identities.collectAsState()
//        val identitiesBeingSaved by viewModel.identitiesBeingSaved.collectAsState()

        PermissionRequester(activity, viewModel)

        cameraProvider?.let {
            CameraPreview(
                activity, it, Modifier
                    .fillMaxSize(), viewModel
            )
        }

        // Show frozen frame overlay when processing
        frozenFrameBitmap?.let { bitmap ->
            Image(
                bitmap = bitmap.asImageBitmap(),
                contentDescription = "Frozen camera frame",
                modifier = Modifier.fillMaxSize(),
                contentScale = ContentScale.Crop
            )
        }

        LaunchedEffect(selectedIdentity, isProcessingIdentity) {
            if (selectedIdentity != null || isProcessingIdentity) {
                activity.freezeCamera()
            } else {
                activity.unfreezeCamera()
            }
        }

//        selectedIdentity?.let {
//            IdentityConfirmationDialog(it, viewModel) {
//                viewModel.dismissIdentity()
//            }
//        }

        if (isProcessingIdentity) {
            IdentityProcessingDialog(
                identityName = processingIdentityName,
                backPressedOnce = backPressedOnce,
                viewModel = viewModel
            )
        }

        LazyColumn(
            Modifier.align(BiasAlignment(0f, 0.65f)),
            horizontalAlignment = Alignment.CenterHorizontally
        ) {
            items(identities, { identity -> identity.ipk.toHexString() }) {
                IdentityActionButton(
                    it,
                    viewModel,
                    Modifier.animateItem(
                        fadeInSpec = null
                    )
                ) {

                }
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


@Composable
private fun IdentityActionButton(
    userIdentity: Identity,
    viewModel: QrScannerVM,
    modifier: Modifier = Modifier,
    onClick: () -> Unit
) {
    val selectedIdentity by viewModel.selectedIdentity.collectAsState()

    val isNew = true // user.isNew
    val name = userIdentity.nickname.let { if (it.isNullOrBlank()) "Anonymous" else it }

    val saving by remember { derivedStateOf { selectedIdentity == userIdentity } }

    Button({
        viewModel.saveUserIdentity(userIdentity)
        onClick()
    }, modifier, enabled = !saving) {
        Row(
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            if (saving) LoadingIndicator(
                Modifier.size(24.dp),
                color = LocalContentColor.current
            )
            else Icon(
                painter = if (isNew) painterResource(R.drawable.i_user_add) else painterResource(R.drawable.i_user_check),
                if (isNew) "Add Contact" else "Contact Saved"
            )

            Text(buildAnnotatedString {
                append("Add ")
                withStyle(style = SpanStyle(fontWeight = FontWeight.Bold)) {
                    append(name)
                }
            })
        }
    }
}


@Composable
private fun IdentityProcessingDialog(
    identityName: String?,
    backPressedOnce: Boolean,
    viewModel: QrScannerVM
) {
    // Handle back press with double-tap to cancel
    BackHandler {
        viewModel.onBackPressedDuringProcessing()
    }

    Dialog(
        onDismissRequest = { viewModel.onBackPressedDuringProcessing() },
        properties = DialogProperties(
            dismissOnBackPress = false, // We handle back press ourselves via BackHandler
            dismissOnClickOutside = false
        )
    ) {
        Card(
            shape = RoundedCornerShape(28.dp),
            elevation = CardDefaults.cardElevation(8.dp),
            colors = CardDefaults.cardColors(
                containerColor = MaterialTheme.colorScheme.surfaceContainerHigh
            ),
            modifier = Modifier.fillMaxWidth()
        ) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(28.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.spacedBy(16.dp)
            ) {
                LoadingIndicator(
                    modifier = Modifier.size(48.dp),
                    color = MaterialTheme.colorScheme.primary
                )

                Column(
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.spacedBy(4.dp)
                ) {
                    Text(
                        "Connecting",
                        style = MaterialTheme.typography.titleLargeEmphasized,
                        color = MaterialTheme.colorScheme.onSurface
                    )

                    identityName?.let { name ->
                        Text(
                            "to $name",
                            style = MaterialTheme.typography.bodyLarge,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            textAlign = TextAlign.Center
                        )
                    }
                }

                Text(
                    "Establishing secure connection...",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.outline,
                    textAlign = TextAlign.Center
                )

                // Show cancel hint
                TextButton(onClick = { viewModel.dismissProcessing() }) {
                    Text("Cancel")
                }
            }
        }
    }
}
