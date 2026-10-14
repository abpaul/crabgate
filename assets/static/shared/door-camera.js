(() => {
  const video = document.getElementById("camera");
  const status = document.getElementById("camera-status");
  const form = document.querySelector("form.manual");
  const code = document.getElementById("code");
  if (!video || !form || !code || !navigator.mediaDevices?.getUserMedia) {
    if (status) status.textContent = "Camera unavailable. Type the code below.";
    return;
  }

  let stream;
  let detector;
  let stopped = false;
  let lastFrame = 0;
  const canvas = document.createElement("canvas");
  const context = canvas.getContext("2d", { willReadFrequently: true });

  async function scan(time) {
    if (stopped) return;
    requestAnimationFrame(scan);
    if (time - lastFrame < 120 || video.readyState < 2) return;
    lastFrame = time;
    try {
      let value;
      if (detector) {
        const results = await detector.detect(video);
        value = results.find((item) => item.rawValue)?.rawValue;
      } else if (context && window.jsQR) {
        canvas.width = video.videoWidth;
        canvas.height = video.videoHeight;
        context.drawImage(video, 0, 0);
        const frame = context.getImageData(0, 0, canvas.width, canvas.height);
        value = window.jsQR(frame.data, frame.width, frame.height)?.data;
      }
      if (value && !stopped) {
        stopped = true;
        code.value = value;
        stream?.getTracks().forEach((track) => track.stop());
        form.requestSubmit();
      }
    } catch {
      detector = null;
    }
  }

  async function start() {
    if (window.BarcodeDetector) {
      try {
        const formats = await window.BarcodeDetector.getSupportedFormats();
        if (formats.includes("qr_code")) detector = new window.BarcodeDetector({ formats: ["qr_code"] });
      } catch {
        detector = null;
      }
    }
    try {
      stream = await navigator.mediaDevices.getUserMedia({ video: { facingMode: "environment" }, audio: false });
      video.srcObject = stream;
      await video.play();
      requestAnimationFrame(scan);
    } catch {
      status.textContent = "Camera unavailable. Type the code below.";
    }
  }

  window.addEventListener("pagehide", () => {
    stopped = true;
    stream?.getTracks().forEach((track) => track.stop());
  });
  start();
})();
