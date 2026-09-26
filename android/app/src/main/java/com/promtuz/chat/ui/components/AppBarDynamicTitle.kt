package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ProvideTextStyle
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.PlatformTextStyle
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.LineHeightStyle
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.sp
import com.promtuz.chat.presentation.state.ConnectionState
import com.promtuz.chat.ui.animation.QueuedAnimatedContent
import com.promtuz.chat.ui.text.ttCommonsProFamily
import com.promtuz.core.CoreBridge
import kotlinx.coroutines.flow.StateFlow

@Composable
internal fun connectionLabel(): String? {
    val connection by CoreBridge.connection.collectAsState()
    return when (connection) {
        ConnectionState.Connected, ConnectionState.Idle -> null
        else -> stringResource(connection.text)
    }
}

/** Shared queued transition for home, screen titles and chat subtitles. */
@Composable
internal fun AnimatedBarLabel(
    text: String,
    modifier: Modifier = Modifier,
    style: TextStyle = MaterialTheme.typography.titleLarge.copy(fontFamily = ttCommonsProFamily, fontWeight = FontWeight.Bold, fontSize = 26.sp),
    alignment: Alignment = Alignment.CenterStart,
    duration: Int = 300,
) {
    QueuedAnimatedContent(text, modifier, durationMillis = duration,
        contentAlignment = alignment, label = "bar label") { value ->
        Text(value, style = style, maxLines = 1, overflow = TextOverflow.Ellipsis)
    }
}

@Composable
fun AppBarDynamicTitle(titles: StateFlow<String>, modifier: Modifier = Modifier, baseDuration: Int = 300) {
    val title by titles.collectAsState()
    AnimatedBarLabel(title, modifier.fillMaxWidth(), alignment = Alignment.Center, duration = baseDuration)
}

@Composable
internal fun screenTitleStyle() = MaterialTheme.typography.titleLarge.copy(
    fontFamily = ttCommonsProFamily,
    fontWeight = FontWeight.SemiBold,
    fontSize = 22.sp,
    platformStyle = PlatformTextStyle(includeFontPadding = false),
    lineHeightStyle = LineHeightStyle(
        alignment = LineHeightStyle.Alignment.Center,
        trim = LineHeightStyle.Trim.Both,
    )
)

@Composable
internal fun ConnectionAwareTitle(title: @Composable () -> Unit) {
    val label = connectionLabel()
    ProvideTextStyle(screenTitleStyle()) {
        QueuedAnimatedContent(label, label = "screen connection title") { status ->
            if (status == null) title() else Text(status, maxLines = 1, overflow = TextOverflow.Ellipsis)
        }
    }
}
