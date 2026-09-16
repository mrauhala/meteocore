"use strict";

window.ui = SwaggerUIBundle({
  url: document.getElementById("swagger-ui").dataset.specUrl,
  dom_id: "#swagger-ui",
  deepLinking: true,
  presets: [SwaggerUIBundle.presets.apis],
  layout: "BaseLayout",
  // Do not send the specification to an external validation service.
  validatorUrl: null,
});
