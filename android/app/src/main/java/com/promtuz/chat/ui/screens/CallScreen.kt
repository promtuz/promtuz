package com.promtuz.chat.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.ui.components.Avatar
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.chat.utils.media.rememberAvatar
import com.promtuz.core.call.CallController
import com.promtuz.core.call.CallController.Phase
import com.promtuz.core.call.CallVideoManager
import kotlinx.coroutines.delay

/**
 * The whole call, one screen. The top half says who and how the call is doing;
 * the bottom half is the controls, which are answer/decline while it rings and
 * mute/speaker/end once it is up.
 */
@Composable
fun CallScreen(call: CallController.Ui?) {
    if (call == null) return
    val colors = MaterialTheme.colorScheme
    val onVideo = call.video && call.phase == Phase.Connected
    Box(
        Modifier
            .fillMaxSize()
            .background(if (onVideo) Color.Black else colors.surface),
    ) {
        // Remote video fills the screen when it is a connected video call and
        // the peer's camera is on; otherwise the peer's avatar and name.
        if (onVideo && call.peerCamera) {
            CallSurface(Modifier.fillMaxSize()) { CallVideoManager.setRemoteSurface(it) }
        }

        Box(Modifier.fillMaxSize().windowInsetsPadding(WindowInsets.safeDrawing)) {
        if (!onVideo || !call.peerCamera) {
            Column(
                Modifier.fillMaxWidth().align(Alignment.TopCenter).padding(top = 72.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Avatar(
                    name = call.name.ifEmpty { "?" },
                    size = 128.dp,
                    image = rememberAvatar(call.peer.toHex()),
                )
                Spacer(Modifier.height(24.dp))
                Text(
                    call.name.ifEmpty { "Unknown" },
                    style = MaterialTheme.typography.headlineMedium,
                    color = if (onVideo) Color.White else colors.onSurface,
                    textAlign = TextAlign.Center,
                )
                Spacer(Modifier.height(8.dp))
                Text(
                    statusLine(call),
                    style = MaterialTheme.typography.bodyLarge,
                    color = if (onVideo) Color.White.copy(alpha = 0.8f) else colors.onSurfaceVariant,
                )
                if (call.peerMuted) {
                    Spacer(Modifier.height(4.dp))
                    Text("Muted", style = MaterialTheme.typography.labelMedium,
                        color = if (onVideo) Color.White.copy(alpha = 0.8f) else colors.onSurfaceVariant)
                }
            }
        }

        // Local self-view, a small tile top-right, while our camera is on.
        val cameraOn by CallVideoManager.cameraOn.collectAsState()
        if (onVideo && cameraOn) {
            CallSurface(
                Modifier
                    .align(Alignment.TopEnd)
                    .padding(16.dp)
                    .size(108.dp, 160.dp)
                    .clip(RoundedCornerShape(12.dp)),
            ) { CallVideoManager.setLocalSurface(it) }
        }

        Box(Modifier.fillMaxWidth().align(Alignment.BottomCenter).padding(bottom = 56.dp)) {
            if (!call.outgoing && (call.phase == Phase.Incoming || call.phase == Phase.Ringing)) {
                IncomingControls()
            } else {
                OngoingControls(call)
            }
        }
        }
    }
}

@Composable
private fun statusLine(call: CallController.Ui): String = when (call.phase) {
    Phase.Outgoing -> "Calling…"
    Phase.Ringing -> "Ringing…"
    Phase.Incoming -> "Incoming call"
    Phase.Connecting -> "Connecting…"
    Phase.Reconnecting -> "Reconnecting…"
    Phase.Connected -> {
        var now by remember { mutableLongStateOf(android.os.SystemClock.elapsedRealtime()) }
        LaunchedEffect(call.callId) {
            while (true) {
                now = android.os.SystemClock.elapsedRealtime()
                delay(500)
            }
        }
        val secs = ((now - call.connectedAt) / 1000).coerceAtLeast(0)
        "%d:%02d".format(secs / 60, secs % 60)
    }
}

@Composable
private fun IncomingControls() {
    Row(
        Modifier.fillMaxWidth().padding(horizontal = 48.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        RoundButton(R.drawable.i_phone, "Decline", Color(0xFFE5484D), Color.White) {
            com.promtuz.core.CoreBridge.callReject()
        }
        RoundButton(R.drawable.i_phone, "Answer", Color(0xFF30A46C), Color.White) {
            com.promtuz.core.CoreBridge.callAccept()
        }
    }
}

@Composable
private fun OngoingControls(call: CallController.Ui) {
    val colors = MaterialTheme.colorScheme
    val cameraOn by CallVideoManager.cameraOn.collectAsState()
    Row(
        Modifier.fillMaxWidth().padding(horizontal = 32.dp),
        horizontalArrangement = Arrangement.SpaceEvenly,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        val muteBg = if (call.muted) colors.onSurface else colors.surfaceVariant
        val muteFg = if (call.muted) colors.surface else colors.onSurface
        RoundButton(
            if (call.muted) R.drawable.i_mic_off else R.drawable.i_mic,
            "Mute", muteBg, muteFg,
        ) { CallController.toggleMute() }

        if (call.video && call.phase == Phase.Connected) {
            val camBg = if (cameraOn) colors.surfaceVariant else colors.onSurface
            val camFg = if (cameraOn) colors.onSurface else colors.surface
            RoundButton(R.drawable.oi_camera, "Camera", camBg, camFg) { CallController.toggleCamera() }
            RoundButton(R.drawable.i_refresh, "Flip", colors.surfaceVariant, colors.onSurface) {
                CallController.switchCamera()
            }
        }

        RoundButton(R.drawable.i_phone, "End", Color(0xFFE5484D), Color.White) {
            com.promtuz.core.CoreBridge.callHangup()
        }

        val spkBg = if (call.speaker) colors.onSurface else colors.surfaceVariant
        val spkFg = if (call.speaker) colors.surface else colors.onSurface
        RoundButton(R.drawable.i_speaker, "Speaker", spkBg, spkFg) { CallController.toggleSpeaker() }
    }
}

/**
 * A `SurfaceView` for call video, handing its `Surface` to the caller as it
 * comes and goes so the encoder or decoder can bind it.
 */
@Composable
private fun CallSurface(modifier: Modifier = Modifier, onSurface: (android.view.Surface?) -> Unit) {
    androidx.compose.ui.viewinterop.AndroidView(
        modifier = modifier,
        factory = { ctx ->
            android.view.SurfaceView(ctx).apply {
                holder.addCallback(object : android.view.SurfaceHolder.Callback {
                    override fun surfaceCreated(holder: android.view.SurfaceHolder) = onSurface(holder.surface)
                    override fun surfaceChanged(h: android.view.SurfaceHolder, f: Int, w: Int, ht: Int) {}
                    override fun surfaceDestroyed(holder: android.view.SurfaceHolder) = onSurface(null)
                })
            }
        },
    )
}

@Composable
private fun RoundButton(icon: Int, label: String, bg: Color, fg: Color, onClick: () -> Unit) {
    Column(horizontalAlignment = Alignment.CenterHorizontally) {
        Box(
            Modifier
                .size(68.dp)
                .clip(CircleShape)
                .background(bg)
                .clickable(onClick = onClick),
            contentAlignment = Alignment.Center,
        ) {
            DrawableIcon(icon, desc = label, tint = fg, size = 28.dp)
        }
    }
}
