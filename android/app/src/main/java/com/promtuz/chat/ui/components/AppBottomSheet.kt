package com.promtuz.chat.ui.components

import androidx.compose.material3.MaterialTheme

import androidx.activity.compose.PredictiveBackHandler
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.BottomSheetDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.ModalBottomSheetProperties
import androidx.compose.material3.SheetState
import androidx.compose.material3.SheetValue
import androidx.compose.material3.Surface
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.RectangleShape
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch
import kotlin.math.min

private val BackDropMax = 96.dp
private const val BackDropFraction = 0.15f
private const val BackSettleMs = 260

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun AppBottomSheet(
    onDismissRequest: () -> Unit,
    modifier: Modifier = Modifier,
    sheetState: SheetState = rememberModalBottomSheetState(),
    dismissEnabled: Boolean = true,
    contentWindowInsets: @Composable () -> WindowInsets = { BottomSheetDefaults.windowInsets },
    content: @Composable ColumnScope.() -> Unit,
) {
    ModalBottomSheet(
        onDismissRequest = onDismissRequest,
        modifier = modifier,
        sheetState = sheetState,
        shape = RectangleShape,
        containerColor = Color.Transparent,
        dragHandle = null,
        contentWindowInsets = { WindowInsets(0) },
        properties = ModalBottomSheetProperties(shouldDismissOnBackPress = false),
    ) {
        val scope = rememberCoroutineScope()
        val drop = remember { Animatable(0f) }
        PredictiveBackHandler(sheetState.targetValue != SheetValue.Hidden) { events ->
            try {
                events.collect { drop.snapTo(it.progress) }
                if (dismissEnabled) {
                    scope.launch { sheetState.hide() }
                        .invokeOnCompletion { if (!sheetState.isVisible) onDismissRequest() }
                } else drop.animateTo(0f, tween(BackSettleMs))
            } catch (e: CancellationException) {
                drop.animateTo(0f, tween(BackSettleMs))
            }
        }
        Surface(
            Modifier
                .fillMaxWidth()
                .graphicsLayer {
                    translationY =
                        drop.value * min(size.height * BackDropFraction, BackDropMax.toPx())
                },
            shape = BottomSheetDefaults.ExpandedShape,
            color = MaterialTheme.colorScheme.surfaceContainer,
        ) {
            Column(Modifier.windowInsetsPadding(contentWindowInsets())) {
                Box(
                    Modifier
                        .fillMaxWidth()
                        .padding(vertical = 8.dp),
                    contentAlignment = Alignment.Center
                ) {
                    Box(
                        Modifier
                            .width(48.dp)
                            .height(3.dp)
                            .clip(
                                MaterialTheme.shapes.extraLarge
                            )
                            .background(MaterialTheme.colorScheme.onSurfaceVariant)
                    )
                }
                content()
            }
        }
    }
}
