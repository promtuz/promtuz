package com.promtuz.chat.ui.camera

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.promtuz.chat.R
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.media.mediaOrigin

/**
 * First cell of the attach grid: the live camera. It warms the shared
 * [CameraSession] and shows its view; opening the full camera takes that same view over,
 * so there is no second start and no black frame. The session is let go once neither the
 * tile nor the full camera shows it.
 */
@Composable
fun CameraTile(onOpen: () -> Unit) {
    val context = LocalContext.current
    val owner = LocalLifecycleOwner.current
    val granted = ContextCompat.checkSelfPermission(context, android.Manifest.permission.CAMERA) ==
        android.content.pm.PackageManager.PERMISSION_GRANTED
    val fullOpen = CameraLauncher.request != null

    if (granted) {
        LaunchedEffect(Unit) { CameraSession.bind(context, owner) }
        DisposableEffect(Unit) { onDispose { if (CameraLauncher.request == null) CameraSession.release() } }
    }

    Box(
        Modifier
            .aspectRatio(1f)
            .padding(1.dp)
            .clip(RoundedCornerShape(4.dp))
            .mediaOrigin(CameraLauncher.TILE_ORIGIN, 4.dp)
            .background(Color.Black)
            .clickable(onClick = onOpen),
    ) {
        if (granted && !fullOpen) AndroidView({ CameraSession.previewView(it) }, Modifier.fillMaxSize())
        DrawableIcon(
            R.drawable.oi_camera, Modifier.align(Alignment.TopStart).padding(8.dp).size(20.dp),
            desc = "Camera", tint = Color.White,
        )
    }
}
