package com.promtuz.chat.ui.components

import androidx.compose.animation.core.Animatable
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.layout.layout
import com.promtuz.chat.ui.stage.SendTransition
import kotlinx.coroutines.CoroutineScope
import kotlin.math.roundToInt

@Stable
internal class ComposerTextExit(private val scope: CoroutineScope, private val onPrepared: (SendTransition) -> Unit) {
    var transition by mutableStateOf<SendTransition?>(null)
        private set
    internal var settledHeight = 0
    internal var startHeight: Int? = null
        private set
    var fading = false
        internal set
    var hiding by mutableStateOf(false)
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
        startHeight = null
        hiding = false
    }

    /** [send] returns whether the send started; until core clears the input, its text is hidden. */
    fun submit(send: () -> Boolean) {
        startHeight = settledHeight
        transition = SendTransition().also(onPrepared)
        hiding = send()
    }
}

/** The sent text leaves at once; only the field's height eases down with the send. */
@Composable
internal fun rememberComposerTextExit(sentRevision: Long, input: String,
    onPrepared: (SendTransition) -> Unit = {},
): ComposerTextExit {
    val scope = rememberCoroutineScope()
    val prepare by rememberUpdatedState(onPrepared)
    val state = remember { ComposerTextExit(scope) { prepare(it) } }
    val fallback = remember { Animatable(1f) }
    val progress = state.transition?.progress ?: fallback
    var previousRevision by remember { mutableLongStateOf(sentRevision) }
    var exiting by remember { mutableStateOf(false) }
    if (sentRevision != previousRevision) {
        previousRevision = sentRevision
        exiting = input.isEmpty() && state.startHeight != null
    }
    if (input.isNotEmpty()) exiting = false
    LaunchedEffect(input.isEmpty()) { if (input.isEmpty()) state.hiding = false }
    LaunchedEffect(sentRevision) {
        // Each transaction keeps running when the next draft or send starts.
        // Otherwise fast consecutive sends would freeze the previous bubble.
        state.accept()
    }
    state.fading = exiting && progress.value < 1f
    state.modifier = Modifier.layout { measurable, constraints ->
        val p = measurable.measure(constraints)
        val shrinking = exiting && progress.value < 1f
        if (!shrinking) state.settledHeight = p.height
        val oldHeight = state.startHeight ?: p.height
        val height = if (shrinking) (oldHeight + (p.height - oldHeight) * progress.value)
            .roundToInt().coerceIn(constraints.minHeight, constraints.maxHeight) else p.height
        layout(p.width, height) { p.place(0, height - p.height) }
    }
    return state
}
