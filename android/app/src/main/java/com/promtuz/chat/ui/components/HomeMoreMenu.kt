package com.promtuz.chat.ui.components

import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.runtime.MutableState
import androidx.compose.runtime.remember
import androidx.compose.ui.*
import androidx.navigation3.runtime.NavKey
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.utils.extensions.then


private data class MenuItem(val label: String, val icon: Int, val onClick: (() -> Unit))

/**
 * FIXME:
 *  It looks like shit right now
 */
@Composable
fun HomeMoreMenu(viewModel: AppVM, expanded: MutableState<Boolean>, modifier: Modifier = Modifier) {
    val open = remember {
        { route: NavKey ->
            expanded.value = false
            viewModel.navigator.push(route)
        }
    }

    val buttonGroups: List<List<MenuItem>> = remember {
        listOf(
            listOf(
                MenuItem("My Profile", R.drawable.i_profile) {}
            ), listOf(
                MenuItem("Settings", R.drawable.i_settings) { open(Routes.Settings) },
            )
        )
    }

    DropdownMenu(
        expanded = expanded.value,
        onDismissRequest = { expanded.value = false },
        modifier = modifier
    ) {
        buttonGroups.forEachIndexed { index, items ->
            (index != 0).then {
                HorizontalDivider()
            }
            items.forEach { item ->
                DropdownMenuItem(
                    text = { Text(item.label) },
                    onClick = { item.onClick() },
                    leadingIcon = { DrawableIcon(item.icon) }
                )
            }
        }
    }
}