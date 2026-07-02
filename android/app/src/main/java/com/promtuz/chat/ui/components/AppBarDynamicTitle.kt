package com.promtuz.chat.ui.components

//import com.promtuz.chat.data.remote.ConnectionStatus
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.core.updateTransition
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.unit.sp
import com.promtuz.chat.ui.constants.Tweens
import com.promtuz.chat.ui.text.calSansfamily
import kotlinx.coroutines.flow.StateFlow

@Composable
fun AppBarDynamicTitle(
    titles: StateFlow<String>, modifier: Modifier = Modifier, baseDuration: Int = 300
) {
    val external by titles.collectAsState()

    var displayed by remember { mutableStateOf(external) }
    var pending by remember { mutableStateOf<String?>(null) }

    var animStart = remember { 0L }
    var animDuration by remember { mutableStateOf(baseDuration) }

    val transition = updateTransition(displayed, label = "titleTransition")
    val animating by remember { derivedStateOf { transition.currentState != transition.targetState } }

    // Feed titles
    LaunchedEffect(external) {
        if (!animating) {
            displayed = external
            animStart = System.currentTimeMillis()
            animDuration = baseDuration
        } else {
            pending = external
        }
    }

    // When a hop finishes, check if there's a pending value.
    LaunchedEffect(animating) {
        if (!animating && pending != null) {
            val elapsed = System.currentTimeMillis() - animStart
            val remaining =
                (baseDuration - elapsed).coerceAtLeast(80).toInt() // never too slow / too fast

            animDuration = remaining
            animStart = System.currentTimeMillis()

            displayed = pending!!
            pending = null
        }
    }

    transition.AnimatedContent(
        modifier = modifier.fillMaxWidth(), contentAlignment = Alignment.Center, transitionSpec = {
            (slideInVertically(
                Tweens.microInteraction(animDuration),
                { it },
            ) + fadeIn(Tweens.microInteraction(animDuration))) togetherWith (slideOutVertically(
                Tweens.microInteraction(animDuration),
                { -it },
            ) + fadeOut(Tweens.microInteraction(animDuration)))
        }) { t ->
        Text(
            t,
            fontFamily = calSansfamily,
            fontSize = 26.sp,
            modifier = Modifier.graphicsLayer { clip = false })
    }
}
