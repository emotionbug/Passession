package sh.paseo.sessions;

import android.content.AsyncQueryHandler;
import android.content.ComponentName;
import android.content.ContentValues;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Looper;
import android.util.Log;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;

/**
 * Launcher icon badges, ported from app_badge_plus 1.3.5 (the Flutter client's
 * plugin) so the same launchers receive the same calls:
 * {@code if (AppBadgePlus.isSupported()) AppBadgePlus.updateBadge(count)}.
 *
 * Portions Copyright (c) 2024 LioLin, MIT License (app_badge_plus):
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions: The above copyright
 * notice and this permission notice shall be included in all copies or
 * substantial portions of the Software. THE SOFTWARE IS PROVIDED "AS IS",
 * WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED.
 */
final class PaseoLauncherBadge {
    private static final String TAG = "Badge";
    private static final String FALLBACK_HOME_PACKAGE = "com.android.settings";
    private static final String FALLBACK_HOME_CLASS = "com.android.settings.FallbackHome";

    /** One launcher family: its home packages and how it takes a count. */
    private interface Launcher {
        List<String> packages();

        void update(Context context, int count) throws Exception;
    }

    private static Launcher badge;
    private static Boolean supported;
    private static AsyncQueryHandler sonyQueryHandler;

    private PaseoLauncherBadge() {}

    /** AppBadgePlus.isSupported (three attempts; stock Pixel launchers use dots only). */
    static synchronized boolean isSupported(Context context) {
        if (supported == null) {
            for (int attempt = 0; attempt < 3; attempt++) {
                if (initBadge(context)) {
                    if (badge == NEXUS) return false;
                    update(context, 0);
                    supported = true;
                    break;
                } else {
                    supported = false;
                }
            }
        }
        return supported != null && supported;
    }

    /** AppBadgePlus.updateBadge: failures are logged, never thrown. */
    static synchronized void update(Context context, int count) {
        try {
            if (badge == null && !initBadge(context)) {
                Log.e(TAG, "Unable to update badge: no default launcher available");
                return;
            }
            badge.update(context, count);
        } catch (Exception e) {
            Log.e(TAG, "Unable to update badge", e);
        }
    }

    private static boolean initBadge(Context context) {
        if (context.getPackageManager().getLaunchIntentForPackage(context.getPackageName()) == null) {
            Log.e(TAG, "Unable to find launch intent for package: " + context.getPackageName());
            return false;
        }
        for (ResolveInfo info : launchers(context)) {
            String home = info == null || info.activityInfo == null ? null : info.activityInfo.packageName;
            for (Launcher launcher : LAUNCHERS) {
                if (launcher.packages().contains(home)) {
                    badge = launcher;
                    break;
                }
            }
            if (badge != null) break;
        }
        return badge != null;
    }

    /** Home activities with the default launcher first. */
    private static List<ResolveInfo> launchers(Context context) {
        Intent intent = new Intent(Intent.ACTION_MAIN);
        intent.addCategory(Intent.CATEGORY_HOME);
        PackageManager manager = context.getPackageManager();
        int flags = Build.VERSION.SDK_INT >= 23 ? PackageManager.MATCH_ALL : 0;
        List<ResolveInfo> list = new ArrayList<>(manager.queryIntentActivities(intent, flags));
        ResolveInfo preferred = manager.resolveActivity(intent, PackageManager.MATCH_DEFAULT_ONLY);
        if (list.isEmpty() || (preferred != null && isFallbackHome(preferred))) return list;
        int swap = 0;
        for (int i = 0; i < list.size(); i++) {
            ResolveInfo item = list.get(i);
            if (preferred != null && preferred.activityInfo != null
                    && item.activityInfo.packageName.equals(preferred.activityInfo.packageName)
                    && item.activityInfo.name.equals(preferred.activityInfo.name)) {
                swap = i;
                break;
            }
        }
        Collections.swap(list, 0, swap);
        return list;
    }

    private static boolean isFallbackHome(ResolveInfo info) {
        return info.activityInfo != null
                && FALLBACK_HOME_PACKAGE.equals(info.activityInfo.packageName)
                && FALLBACK_HOME_CLASS.equals(info.activityInfo.name);
    }

    private static ComponentName component(Context context) {
        Intent launch = context.getPackageManager().getLaunchIntentForPackage(context.getPackageName());
        return launch == null ? null : launch.getComponent();
    }

    private static String className(Context context) {
        ComponentName component = component(context);
        return component == null ? "" : component.getClassName();
    }

    /** BroadcastTool.sendBroadcast: explicit to every receiver that resolves it. */
    private static void sendExplicit(Context context, Intent intent) {
        List<ResolveInfo> receivers = context.getPackageManager().queryBroadcastReceivers(intent, 0);
        if (receivers.isEmpty()) {
            Log.e(TAG, "unable to resolve intent: " + intent);
            return;
        }
        for (ResolveInfo receiver : receivers) {
            Intent explicit = new Intent(intent);
            explicit.setPackage(receiver.resolvePackageName);
            context.sendBroadcast(explicit);
        }
    }

    /** BroadcastTool.sendDefaultBroadcast. */
    private static void sendDefault(Context context, Intent intent) {
        if (Build.VERSION.SDK_INT >= 26) {
            Intent oreo = new Intent(intent);
            oreo.setAction("me.hacket.launcherbadge.BADGE_COUNT_UPDATE");
            try {
                sendExplicit(context, oreo);
                return;
            } catch (Exception ignored) {
            }
        }
        sendExplicit(context, intent);
    }

    private static Launcher launcher(List<String> packages, BadgeUpdate update) {
        return new Launcher() {
            public List<String> packages() {
                return packages;
            }

            public void update(Context context, int count) throws Exception {
                update.apply(context, count);
            }
        };
    }

    private interface BadgeUpdate {
        void apply(Context context, int count) throws Exception;
    }

    private static final Launcher DEFAULT = launcher(Collections.emptyList(), (context, count) -> {
        // Notification dots (API 26+) need nothing from the app.
    });

    private static final Launcher NEXUS = launcher(Collections.singletonList("com.google.android.apps.nexuslauncher"), (context, count) ->
            Log.d(TAG, "Nexus Launcher does not support badge count. Please use notification dots instead."));

    private static final Launcher APEX = launcher(Collections.singletonList("com.anddoes.launcher"), (context, count) -> {
        ComponentName component = component(context);
        Intent intent = new Intent("com.anddoes.launcher.COUNTER_CHANGED");
        intent.putExtra("package", component == null ? null : component.getPackageName());
        intent.putExtra("class", component == null ? null : component.getClassName());
        intent.putExtra("count", count);
        sendExplicit(context, intent);
    });

    private static final Launcher ASUS = launcher(Collections.singletonList("com.asus.launcher"), (context, count) -> {
        ComponentName component = component(context);
        Intent intent = new Intent("android.intent.action.BADGE_COUNT_UPDATE");
        intent.putExtra("badge_count", count);
        intent.putExtra("badge_count_package_name", component == null ? null : component.getPackageName());
        intent.putExtra("badge_count_class_name", component == null ? null : component.getClassName());
        intent.putExtra("badge_vip_count", 0);
        context.sendBroadcast(intent);
    });

    private static final Launcher HTC = launcher(Collections.singletonList("com.htc.launcher"), (context, count) -> {
        ComponentName component = component(context);
        Intent notification = new Intent("com.htc.launcher.action.SET_NOTIFICATION");
        notification.putExtra("com.htc.launcher.extra.COMPONENT", component == null ? null : component.flattenToShortString());
        notification.putExtra("com.htc.launcher.extra.COUNT", count);
        try {
            sendExplicit(context, notification);
        } catch (Exception e) {
            Log.e(TAG, "unable to resolve intent: " + notification, e);
        }
        Intent shortcut = new Intent("com.htc.launcher.action.UPDATE_SHORTCUT");
        shortcut.putExtra("packagename", component == null ? null : component.getPackageName());
        shortcut.putExtra("count", count);
        try {
            sendExplicit(context, shortcut);
        } catch (Exception e) {
            Log.e(TAG, "unable to resolve intent: " + shortcut, e);
        }
    });

    private static void changeBadge(Context context, String authority, int count) {
        Bundle extras = new Bundle();
        extras.putString("package", context.getPackageName());
        extras.putString("class", className(context));
        extras.putInt("badgenumber", count);
        context.getContentResolver().call(Uri.parse("content://" + authority + "/badge/"), "change_badge", null, extras);
    }

    private static final Launcher HUAWEI = launcher(Collections.singletonList("com.huawei.android.launcher"),
            (context, count) -> changeBadge(context, "com.huawei.android.launcher.settings", count));

    private static final Launcher HIHONOR = launcher(Collections.singletonList("com.hihonor.android.launcher"),
            (context, count) -> changeBadge(context, "com.hihonor.android.launcher.settings", count));

    private static final Launcher LG = launcher(Arrays.asList("com.lge.launcher", "com.lge.launcher2"), (context, count) -> {
        ComponentName component = component(context);
        if (component == null) return;
        Intent intent = new Intent("android.intent.action.BADGE_COUNT_UPDATE");
        intent.putExtra("badge_count_package_name", component.getPackageName());
        intent.putExtra("badge_count_class_name", component.getClassName());
        intent.putExtra("badge_count", count);
        sendDefault(context, intent);
    });

    /** MIUI/HyperOS: app_badge_plus 1.3.5 leaves the update disabled (notification-count badges). */
    private static final Launcher MIUI = launcher(Arrays.asList(
            "com.miui.miuilite", "com.miui.home", "com.miui.miuihome", "com.miui.miuihome2",
            "com.miui.mihome", "com.miui.mihome2", "com.mi.android.globallauncher", "com.i.miui.launcher"),
            (context, count) -> {
            });

    private static final Launcher NOWA = launcher(Collections.singletonList("com.teslacoilsw.launcher"), (context, count) -> {
        ComponentName component = component(context);
        ContentValues values = new ContentValues();
        values.put("BADGE", (component == null ? null : component.getPackageName()) + "/" + (component == null ? null : component.getClassName()));
        values.put("count", count);
        context.getContentResolver().insert(Uri.parse("content://com.teslacoilsw.notifier/unread_count"), values);
    });

    private static final Launcher OPPO = launcher(Collections.singletonList("com.oppo.launcher"), (context, count) -> {
        try {
            Bundle extras = new Bundle();
            extras.putInt("app_badge_count", count);
            context.getContentResolver().call(Uri.parse("content://com.android.badge/badge"), "setAppBadgeCount", null, extras);
        } catch (Throwable ignored) {
            Log.e(TAG, "OPPO: Unable to execute Badge By Content Provider", ignored);
        }
    });

    private static final Launcher SAMSUNG = launcher(Arrays.asList("com.sec.android.app.launcher", "com.sec.android.app.twlauncher"), (context, count) -> {
        Intent intent = new Intent("android.intent.action.BADGE_COUNT_UPDATE");
        intent.putExtra("badge_count_package_name", context.getPackageName());
        intent.putExtra("badge_count_class_name", className(context));
        intent.putExtra("badge_count", count);
        context.sendBroadcast(intent);
    });

    private static final Launcher SONY = launcher(Arrays.asList("com.sonyericsson.home", "com.sonymobile.home"), (context, count) -> {
        ComponentName component = component(context);
        if (component == null) return;
        if (context.getPackageManager().resolveContentProvider("com.sonymobile.home.resourceprovider", 0) != null) {
            if (count < 0) return;
            ContentValues values = new ContentValues();
            values.put("badge_count", count);
            values.put("package_name", component.getPackageName());
            values.put("activity_name", component.getClassName());
            Uri uri = Uri.parse("content://com.sonymobile.home.resourceprovider/badge");
            if (Looper.myLooper() == Looper.getMainLooper()) {
                if (sonyQueryHandler == null) {
                    sonyQueryHandler = new AsyncQueryHandler(context.getApplicationContext().getContentResolver()) {};
                }
                sonyQueryHandler.startInsert(0, null, uri, values);
            } else {
                context.getApplicationContext().getContentResolver().insert(uri, values);
            }
        } else {
            Intent intent = new Intent("com.sonyericsson.home.action.UPDATE_BADGE");
            intent.putExtra("com.sonyericsson.home.intent.extra.badge.PACKAGE_NAME", component.getPackageName());
            intent.putExtra("com.sonyericsson.home.intent.extra.badge.ACTIVITY_NAME", component.getClassName());
            intent.putExtra("com.sonyericsson.home.intent.extra.badge.MESSAGE", String.valueOf(count));
            intent.putExtra("com.sonyericsson.home.intent.extra.badge.SHOW_MESSAGE", count > 0);
            context.sendBroadcast(intent);
        }
    });

    private static final Launcher VIVO = launcher(Arrays.asList("com.vivo.launcher", "com.vivo.simplelauncher"), (context, count) -> {
        Intent intent = new Intent("launcher.action.CHANGE_APPLICATION_NOTIFICATION_NUM");
        intent.putExtra("packageName", context.getPackageName());
        intent.putExtra("className", className(context));
        intent.putExtra("notificationNum", count);
        try {
            intent.addFlags(Intent.class.getField("FLAG_RECEIVER_INCLUDE_BACKGROUND").getInt(null));
        } catch (Exception ignored) {
        }
        context.sendBroadcast(intent);
    });

    private static final Launcher YANDEX = launcher(Collections.singletonList("com.yandex.launcher"), (context, count) -> {
        ComponentName component = component(context);
        Bundle extras = new Bundle();
        extras.putString("package", component == null ? null : component.getPackageName());
        extras.putString("class", className(context));
        extras.putString("badges_count", String.valueOf(count));
        context.getContentResolver().call(Uri.parse("content://com.yandex.launcher.badges_external"), "setBadgeNumber", null, extras);
    });

    private static final Launcher ZTE = launcher(Collections.emptyList(), (context, count) -> {
        ComponentName component = component(context);
        Bundle extras = new Bundle();
        extras.putString("app_badge_component_name", component == null ? null : component.flattenToString());
        extras.putInt("app_badge_count", count);
        context.getContentResolver().call(Uri.parse("content://com.android.launcher3.cornermark.unreadbadge"), "setAppUnreadCount", null, extras);
    });

    /** app_badge_plus `Badge.BADGES` order. */
    private static final List<Launcher> LAUNCHERS = Arrays.asList(
            DEFAULT, NEXUS, APEX, ASUS, HTC, HUAWEI, HIHONOR, LG, MIUI, NOWA, OPPO, SAMSUNG, SONY, VIVO, YANDEX, ZTE);
}
