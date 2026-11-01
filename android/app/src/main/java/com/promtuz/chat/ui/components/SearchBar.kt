package com.promtuz.chat.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.positionInRoot
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.toSize
import com.promtuz.chat.R
import com.promtuz.chat.navigation.LocalSearchMotion

/** Search hints inherit exactly the same metrics as the text replacing them. */
@Composable
fun searchTextStyle() = MaterialTheme.typography.bodyLarge.copy(fontWeight = FontWeight.Normal)

/** One visual owner for the list affordance, editable field, and their transition. */
@Composable
fun HomeSearchBar(
    expanded: Boolean = false,
    query: String = "",
    onQueryChange: (String) -> Unit = {},
    focusRequester: FocusRequester? = null,
    onClick: () -> Unit = {},
) {
    val motion = LocalSearchMotion.current
    // Coordinates are consumed by graphics layers, not composition. Scrolling and animation must
    // not recompose the field or the chat list on every frame.
    val bounds = remember { mutableStateOf(Rect.Zero) }
    val colors = MaterialTheme.colorScheme
    Box(
        modifier = Modifier
            .padding(10.dp, 8.dp)
            .fillMaxWidth()
            .height(44.dp)
            .onGloballyPositioned {
                bounds.value = Rect(it.positionInRoot(), it.size.toSize())
            }
            .graphicsLayer {
                if (expanded) {
                    val p = motion.fraction().coerceIn(0f, 1f)
                    val source = motion.origin
                    translationY = if (source != null) (source.top - bounds.value.top) * (1f - p) else 0f
                    // Overflow search has no visible source bar to travel from.
                    alpha = if (source == null) p else 1f
                } else {
                    alpha = if (motion.transitioning) 0f else 1f
                }
            }
            .then(if (expanded) Modifier else Modifier.clickable(
                indication = null, interactionSource = null,
                onClick = {
                    motion.origin = bounds.value
                    onClick()
                },
            ))
            .clip(RoundedCornerShape(28.dp))
            .background(colors.surfaceContainerHigh),
    ) {
        Row(Modifier.fillMaxSize(), verticalAlignment = Alignment.CenterVertically) {
            Box(Modifier.width(44.dp).fillMaxHeight(), contentAlignment = Alignment.Center) {
                DrawableIcon(R.drawable.oi_search, size = 18.dp, strokeWidth = 2.25f,
                    tint = colors.onSurfaceVariant,
                    modifier = Modifier.graphicsLayer {
                        alpha = if (expanded) 1f - motion.fraction().coerceIn(0f, 1f) else 1f
                    })
                if (expanded) {
                    IconButton(onClick = { motion.onBack() }, modifier = Modifier.graphicsLayer {
                        alpha = motion.fraction().coerceIn(0f, 1f)
                    }) {
                        TopBarMorphIcon(MorphGlyph.Back, "Back", tint = colors.onSurfaceVariant, modifier = Modifier.size(20.dp), strokeWidth = 2.25.dp)
                    }
                }
            }
            Box(Modifier.weight(1f).padding(end = 16.dp), contentAlignment = Alignment.CenterStart) {
                if (expanded) {
                    BasicTextField(
                        value = query,
                        onValueChange = onQueryChange,
                        singleLine = true,
                        textStyle = searchTextStyle().copy(color = colors.onSurface),
                        cursorBrush = SolidColor(colors.primary),
                        keyboardOptions = KeyboardOptions(
                            capitalization = KeyboardCapitalization.None,
                            autoCorrectEnabled = false,
                            keyboardType = KeyboardType.Password,
                            imeAction = ImeAction.Search,
                        ),
                        modifier = Modifier.fillMaxWidth().then(
                            if (focusRequester != null) Modifier.focusRequester(focusRequester) else Modifier),
                        decorationBox = { inner ->
                            Box(contentAlignment = Alignment.CenterStart) {
                                if (query.isEmpty()) SearchHint()
                                inner()
                            }
                        },
                    )
                } else SearchHint()
            }
        }
    }
}

@Composable
private fun SearchHint() {
    Text("Search", style = searchTextStyle(), color = MaterialTheme.colorScheme.onSurfaceVariant)
}
