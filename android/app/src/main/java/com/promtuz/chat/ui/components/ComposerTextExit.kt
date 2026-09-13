package com.promtuz.chat.ui.components

import androidx.compose.animation.core.Animatable
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.layer.GraphicsLayer
import androidx.compose.ui.graphics.layer.drawLayer
import androidx.compose.ui.graphics.rememberGraphicsLayer
import androidx.compose.ui.layout.layout
import com.promtuz.chat.ui.stage.ChatMotion
import com.promtuz.chat.ui.stage.SendTransition
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.launch
import kotlin.math.roundToInt
import androidx.compose.ui.geometry.Offset

@Stable
internal class ComposerTextExit(private val layer: GraphicsLayer, private val scope: CoroutineScope, private val onPrepared: (SendTransition) -> Unit) {
    var transition by mutableStateOf<SendTransition?>(null)
        private set
    var capturing by mutableStateOf(false)
        private set
    var bitmap: ImageBitmap? = null
        private set
    var fading = false
        internal set
    var modifier: Modifier = Modifier
        internal set

    fun accept() {
        val accepted = transition ?: return
        accepted.accept(scope)
    }

    fun reject() {
        val rejected = transition ?: return
        rejected.reject(scope)
        bitmap = null
    }

    /** Freeze pixels before changing the native field's mutable text drawing. */
    fun submit(send: () -> Unit) {
        if (capturing) return
        capturing = true
        scope.launch {
            try {
                bitmap = try { layer.toImageBitmap() }
                catch (e: CancellationException) { throw e }
                catch (_: Exception) { null }
                transition = SendTransition().also(onPrepared)
                send()
            } finally { capturing = false }
        }
    }
}

/** Retain the actual field drawing, including its scroll position, for an accepted send. */
@Composable
internal fun rememberComposerTextExit(sentRevision: Long, input: String,
    onPrepared: (SendTransition) -> Unit = {},
): ComposerTextExit {
    val layer = rememberGraphicsLayer()
    val scope = rememberCoroutineScope()
    val prepare by rememberUpdatedState(onPrepared)
    val state = remember { ComposerTextExit(layer, scope) { prepare(it) } }
    val fallback = remember { Animatable(1f) }
    val progress = state.transition?.progress ?: fallback
    var previousRevision by remember { mutableLongStateOf(sentRevision) }
    var exiting by remember { mutableStateOf(false) }
    if (sentRevision != previousRevision) {
        previousRevision = sentRevision
        exiting = input.isEmpty() && state.bitmap != null
    }
    if (input.isNotEmpty()) exiting = false
    LaunchedEffect(sentRevision) {
        // Each transaction keeps running when the next draft or send starts.
        // Otherwise fast consecutive sends would freeze the previous bubble.
        state.accept()
    }
    state.fading = exiting && progress.value < 1f
    state.modifier = Modifier.layout { measurable, constraints ->
        val p = measurable.measure(constraints)
        val oldHeight = state.bitmap?.height ?: p.height
        val height = if (exiting && progress.value < 1f) (oldHeight + (p.height - oldHeight) * progress.value)
            .roundToInt().coerceIn(constraints.minHeight, constraints.maxHeight) else p.height
        layout(p.width, height) { p.place(0, height - p.height) }
    }.drawWithContent {
        if (exiting && progress.value < 1f) {
            drawContent()
            state.bitmap?.let {
                drawImage(it, Offset(0f, size.height - it.height), alpha = 1f - progress.value)
            }
        } else {
            layer.record { this@drawWithContent.drawContent() }
            drawLayer(layer)
        }
    }
    return state
}
