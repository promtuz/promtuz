package com.promtuz.chat.ui.util

import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.graphics.layer.drawLayer
import androidx.compose.ui.graphics.rememberGraphicsLayer
import com.promtuz.chat.navigation.LocalNavCardExiting

/**
 * While [LocalNavCardExiting], replays the last live frame: Haze samples in screen space and
 * shatters under an ancestor scale. Freeze and thaw both land at scale 1, so the swap is seamless.
 */
@Composable
fun Modifier.freezeOnExit(): Modifier {
    val frozen = LocalNavCardExiting.current
    val layer = rememberGraphicsLayer()
    return drawWithContent {
        if (!frozen) layer.record { this@drawWithContent.drawContent() }
        drawLayer(layer)
    }
}
