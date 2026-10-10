package org.rustwindowing.winit;

import android.app.Activity;
import android.content.ClipData;
import android.content.ClipDescription;
import android.content.Intent;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.Point;
import android.net.Uri;
import android.os.Binder;
import android.os.Bundle;
import android.os.IBinder;
import android.os.Parcel;
import android.os.PersistableBundle;
import android.util.Log;
import android.view.DragAndDropPermissions;
import android.view.DragEvent;
import android.view.View;

import java.util.ArrayList;

public final class DragAndDrop implements View.OnDragListener {
    private static final String TAG = "winit";
    private static final String ACTIONS = "org.rustwindowing.winit.actions";
    private static final String REPLY = "org.rustwindowing.winit.reply";
    private static final int MAX_MIME_TYPES = 64;
    private static final int MAX_ITEMS = 256;
    private static final int MAX_CHARS = 8 * 1024 * 1024;
    private static final int ACTION_MOVE = 1;
    private static final int ACTION_COPY = 2;
    private static final int NO_ACTION = 0;

    private final Activity activity;
    private DragAndDropPermissions permissions;

    private DragAndDrop(Activity activity) {
        this.activity = activity;
    }

    private static native void entered(String[] mimeTypes, int[] actions);

    private static native void located(float x, float y);

    private static native void exited();

    private static native int dropped(float x, float y, String[] texts,
            String[] htmls, String[] uris);

    private static native void ended(boolean ours, long drag, boolean result,
            int action);

    private static native void failed(long drag);

    public static void attach(final Activity activity) {
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                try {
                    activity.getWindow().getDecorView()
                            .setOnDragListener(new DragAndDrop(activity));
                } catch (Throwable failure) {
                    Log.w(TAG, "the drag listener could not be attached", failure);
                }
            }
        });
    }

    public static void start(final Activity activity, final long drag,
            final String text, final String html, final String[] uris, final int[] actions,
            final int[] pixels, final int width, final int height, final int touchX,
            final int touchY) {
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                boolean started;
                try {
                    started = begin(activity, drag, text, html, uris, actions, pixels, width,
                            height, touchX, touchY);
                } catch (Throwable failure) {
                    Log.w(TAG, "the drag could not be started", failure);
                    started = false;
                }

                if (!started) {
                    failed(drag);
                }
            }
        });
    }

    private static boolean begin(Activity activity, long drag, String text, String html,
            String[] uris, int[] actions, int[] pixels, int width, int height, int touchX,
            int touchY) {
        View view = activity.getWindow().getDecorView();
        Reply reply = new Reply(drag);

        Bundle extras = new Bundle();
        extras.putBinder(REPLY, reply);
        Intent intent = new Intent();
        intent.putExtras(extras);

        ArrayList<String> mimeTypes = new ArrayList<String>();
        if (text != null) {
            mimeTypes.add(ClipDescription.MIMETYPE_TEXT_PLAIN);
        }

        if (html != null) {
            mimeTypes.add(ClipDescription.MIMETYPE_TEXT_HTML);
        }

        if (uris.length > 0) {
            mimeTypes.add(ClipDescription.MIMETYPE_TEXT_URILIST);
        }

        ClipDescription description =
                new ClipDescription("winit", mimeTypes.toArray(new String[0]));
        PersistableBundle offered = new PersistableBundle();
        offered.putIntArray(ACTIONS, actions);
        description.setExtras(offered);

        String plain = text;
        if (plain == null && html != null) {
            plain = html;
        }

        Uri first = uris.length > 0 ? Uri.parse(uris[0]) : null;
        ClipData clip = new ClipData(description, new ClipData.Item(plain, html, intent, first));
        for (int index = 1; index < uris.length; index++) {
            clip.addItem(new ClipData.Item(Uri.parse(uris[index])));
        }

        Bitmap bitmap;
        if (pixels != null && width > 0 && height > 0) {
            bitmap = Bitmap.createBitmap(pixels, width, height, Bitmap.Config.ARGB_8888);
        } else {
            bitmap = Bitmap.createBitmap(1, 1, Bitmap.Config.ARGB_8888);
        }

        int flags = View.DRAG_FLAG_GLOBAL | View.DRAG_FLAG_GLOBAL_URI_READ;
        return view.startDragAndDrop(clip, new Shadow(bitmap, touchX, touchY), reply, flags);
    }

    @Override
    public boolean onDrag(View view, DragEvent event) {
        try {
            return handle(event);
        } catch (Throwable failure) {
            Log.w(TAG, "a drag event could not be handled", failure);
            return false;
        }
    }

    private boolean handle(DragEvent event) {
        switch (event.getAction()) {
            case DragEvent.ACTION_DRAG_STARTED:
                return true;
            case DragEvent.ACTION_DRAG_ENTERED: {
                ClipDescription description = event.getClipDescription();
                entered(mimeTypes(description), actions(description));
                return true;
            }
            case DragEvent.ACTION_DRAG_LOCATION:
                located(event.getX(), event.getY());
                return true;
            case DragEvent.ACTION_DRAG_EXITED:
                exited();
                return true;
            case DragEvent.ACTION_DROP:
                return drop(event);
            case DragEvent.ACTION_DRAG_ENDED:
                end(event);
                return true;
            default:
                return false;
        }
    }

    private boolean drop(DragEvent event) {
        ClipData clip = event.getClipData();
        ArrayList<String> texts = new ArrayList<String>();
        ArrayList<String> htmls = new ArrayList<String>();
        ArrayList<String> uris = new ArrayList<String>();
        boolean content = false;

        if (clip != null) {
            int count = Math.min(clip.getItemCount(), MAX_ITEMS);
            int budget = MAX_CHARS;
            for (int index = 0; index < count; index++) {
                ClipData.Item item = clip.getItemAt(index);
                CharSequence text = item.getText();
                if (text != null && text.length() <= budget) {
                    texts.add(text.toString());
                    budget -= text.length();
                }

                String html = item.getHtmlText();
                if (html != null && html.length() <= budget) {
                    htmls.add(html);
                    budget -= html.length();
                }

                Uri uri = item.getUri();
                if (uri != null) {
                    String spelled = uri.toString();
                    if (spelled.length() <= budget) {
                        uris.add(spelled);
                        budget -= spelled.length();
                        content |= "content".equals(uri.getScheme());
                    }
                }
            }
        }

        int action = dropped(event.getX(), event.getY(),
                texts.toArray(new String[0]), htmls.toArray(new String[0]),
                uris.toArray(new String[0]));
        if (action == NO_ACTION) {
            return false;
        }

        if (content && event.getLocalState() == null) {
            if (permissions != null) {
                permissions.release();
            }

            permissions = activity.requestDragAndDropPermissions(event);
        }

        answer(clip, action);
        return true;
    }

    private void end(DragEvent event) {
        Object local = event.getLocalState();
        if (local instanceof Reply) {
            Reply reply = (Reply) local;
            ended(true, reply.drag, event.getResult(), reply.action());
        } else {
            ended(false, 0, event.getResult(), NO_ACTION);
        }
    }

    private static void answer(ClipData clip, int action) {
        if (clip == null || clip.getItemCount() == 0) {
            return;
        }

        Intent intent = clip.getItemAt(0).getIntent();
        if (intent == null) {
            return;
        }

        IBinder binder;
        try {
            Bundle extras = intent.getExtras();
            binder = extras == null ? null : extras.getBinder(REPLY);
        } catch (Throwable failure) {
            Log.w(TAG, "the source of the drop could not be read", failure);
            return;
        }

        if (binder == null) {
            return;
        }

        Parcel data = Parcel.obtain();
        try {
            data.writeInt(action);
            binder.transact(IBinder.FIRST_CALL_TRANSACTION, data, null, 0);
        } catch (Throwable failure) {
            Log.w(TAG, "the source of the drop could not be answered", failure);
        } finally {
            data.recycle();
        }
    }

    private static String[] mimeTypes(ClipDescription description) {
        if (description == null) {
            return new String[0];
        }

        int count = Math.min(description.getMimeTypeCount(), MAX_MIME_TYPES);
        String[] mimeTypes = new String[count];
        for (int index = 0; index < count; index++) {
            mimeTypes[index] = description.getMimeType(index);
        }

        return mimeTypes;
    }

    private static int[] actions(ClipDescription description) {
        if (description == null) {
            return new int[0];
        }

        PersistableBundle extras = description.getExtras();
        if (extras == null) {
            return new int[0];
        }

        int[] actions = extras.getIntArray(ACTIONS);
        return actions == null ? new int[0] : actions;
    }

    private static final class Reply extends Binder {
        private final long drag;
        private volatile int action = NO_ACTION;

        Reply(long drag) {
            this.drag = drag;
        }

        int action() {
            return action;
        }

        @Override
        protected boolean onTransact(int code, Parcel data, Parcel reply, int flags)
                throws android.os.RemoteException {
            if (code != FIRST_CALL_TRANSACTION) {
                return super.onTransact(code, data, reply, flags);
            }

            int value = data.readInt();
            if (value == ACTION_MOVE || value == ACTION_COPY) {
                action = value;
            }

            return true;
        }
    }

    private static final class Shadow extends View.DragShadowBuilder {
        private final Bitmap bitmap;
        private final int touchX;
        private final int touchY;

        Shadow(Bitmap bitmap, int touchX, int touchY) {
            super();
            this.bitmap = bitmap;
            this.touchX = touchX;
            this.touchY = touchY;
        }

        @Override
        public void onProvideShadowMetrics(Point size, Point touch) {
            size.set(bitmap.getWidth(), bitmap.getHeight());
            touch.set(touchX, touchY);
        }

        @Override
        public void onDrawShadow(Canvas canvas) {
            canvas.drawBitmap(bitmap, 0, 0, null);
        }
    }
}
