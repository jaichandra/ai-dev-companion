// The Settings panel's form — the in-browser counterpart of the terminal
// setup wizard (companion-service/setup.js), prefilled from GET /settings.
// See companion-service/core/settings.js for what that returns and what
// PUT /settings accepts. This file only builds and reads the form;
// content.js's openSettingsPanel loads, saves and restarts.
(function () {
  const CHECK_LABELS = { git: "git", claudeCli: "Claude Code CLI", claudeAuth: "Claude Code login" };
  // Which features each group of settings belongs to ("jira", "jenkins", "git"): each
  // feature names its groups in its registry entry (`settingsGroups`). A group only shows while
  // one of its features is ticked. Read when the form is built, once every feature has registered.
  function usedByGroups() {
    const groups = {};
    for (const entry of PaiRegistry.all()) {
      for (const group of entry.settingsGroups || []) (groups[group] = groups[group] || []).push(entry.id);
    }
    return groups;
  }

  // Nav icons from the Settings design (masked, so they take the tile's colour).
  const PAGE_ICONS = {
    features: "data:image/svg+xml,%3Csvg viewBox=%220 0 24 24%22 fill=%22none%22 xmlns=%22http://www.w3.org/2000/svg%22%3E%3Cg%3E%3Cpath d=%22M8.86706 4.48327C9.25387 3.43917 10.7303 3.43917 11.1171 4.48327L13.0419 9.68347C13.1634 10.0119 13.4224 10.2709 13.7508 10.3925L18.951 12.3173C19.9951 12.7041 19.9951 14.1805 18.951 14.5673L13.7508 16.4921C13.4224 16.6136 13.1634 16.8726 13.0419 17.201L11.1171 22.4012L11.078 22.496C10.6536 23.4139 9.33056 23.4139 8.90612 22.496L8.86706 22.4012L6.94225 17.201C6.83592 16.9137 6.6246 16.6793 6.35339 16.5438L6.23327 16.4921L1.03307 14.5673C-0.0110245 14.1804 -0.0110248 12.7041 1.03307 12.3173L6.23327 10.3925C6.56173 10.2709 6.82071 10.0119 6.94225 9.68347L8.86706 4.48327ZM8.34948 10.204C8.07601 10.943 7.49282 11.5262 6.75378 11.7997L2.31335 13.4423L6.75378 15.0848C7.44675 15.3413 8.00278 15.8697 8.29479 16.5438L8.34948 16.6805L9.99206 21.12L11.6346 16.6805L11.6893 16.5438C11.9813 15.8697 12.5374 15.3413 13.2303 15.0848L17.6698 13.4423L13.2303 11.7997C12.4913 11.5262 11.9081 10.943 11.6346 10.204L9.99206 5.76354L8.34948 10.204Z%22 fill=%22%23000%22%3E%3C/path%3E%3Cpath d=%22M17.8671 1.48327C18.2539 0.439169 19.7303 0.439171 20.1171 1.48327L20.6913 3.03405C20.8128 3.36252 21.0718 3.62149 21.4003 3.74304L22.951 4.31726C23.9951 4.70406 23.9951 6.18045 22.951 6.56726L21.4003 7.14147C21.0718 7.26302 20.8128 7.522 20.6913 7.85046L20.1171 9.40124L20.078 9.49597C19.6536 10.4139 18.3306 10.4139 17.9061 9.49597L17.8671 9.40124L17.2928 7.85046C17.1865 7.56312 16.9752 7.32873 16.704 7.19323L16.5839 7.14147L15.0331 6.56726C13.989 6.18045 13.989 4.70406 15.0331 4.31726L16.5839 3.74304C16.9123 3.62149 17.1713 3.36252 17.2928 3.03405L17.8671 1.48327ZM18.7001 3.55456C18.4266 4.2936 17.8434 4.87679 17.1044 5.15026L16.3143 5.44226L17.1044 5.73425C17.7972 5.99062 18.3523 6.51929 18.6444 7.19323L18.7001 7.32995L18.9921 8.11901L19.2841 7.32995L19.3397 7.19323C19.6318 6.51929 20.1869 5.99062 20.8798 5.73425L21.6688 5.44226L20.8798 5.15026C20.1407 4.87679 19.5575 4.2936 19.2841 3.55456L18.9921 2.76452L18.7001 3.55456Z%22 fill=%22%23000%22%3E%3C/path%3E%3C/g%3E%3C/svg%3E",
    conn: "data:image/svg+xml,%3Csvg viewBox=%220 0 24 24%22 fill=%22none%22 xmlns=%22http://www.w3.org/2000/svg%22%3E%3Cpath fill-rule=%22evenodd%22 clip-rule=%22evenodd%22 d=%22M23.4013 11.3499L16.9106 4.76929C16.7359 4.59676 16.501 4.5 16.2555 4.5C16.0099 4.5 15.7742 4.59676 15.5995 4.76929C15.4284 4.94747 15.3325 5.18486 15.3325 5.43184C15.3325 5.67882 15.4284 5.91625 15.5995 6.09442L21.4448 12L15.5995 17.9056C15.4284 18.0838 15.3325 18.3212 15.3325 18.5682C15.3325 18.8151 15.4284 19.0525 15.5995 19.2307C15.7742 19.4032 16.0099 19.5 16.2555 19.5C16.501 19.5 16.7359 19.4032 16.9106 19.2307L23.4013 12.6734C23.5719 12.4953 23.6675 12.2583 23.6675 12.0117C23.6675 11.7651 23.5719 11.528 23.4013 11.3499Z%22 fill=%22%23000%22%3E%3C/path%3E%3Cpath fill-rule=%22evenodd%22 clip-rule=%22evenodd%22 d=%22M0.598659 12.6501L7.08937 19.2307C7.26407 19.4032 7.499 19.5 7.74454 19.5C7.99009 19.5 8.22583 19.4032 8.40053 19.2307C8.57157 19.0525 8.66749 18.8151 8.66749 18.5682C8.66749 18.3212 8.57157 18.0838 8.40053 17.9056L2.55523 12L8.40053 6.09442C8.57157 5.91625 8.66749 5.67882 8.66749 5.43184C8.66749 5.18486 8.57157 4.94747 8.40053 4.76929C8.22583 4.59676 7.99009 4.5 7.74454 4.5C7.499 4.5 7.26407 4.59676 7.08937 4.76929L0.598659 11.3266C0.428115 11.5047 0.33252 11.7417 0.33252 11.9883C0.33252 12.2349 0.428115 12.472 0.598659 12.6501Z%22 fill=%22%23000%22%3E%3C/path%3E%3C/svg%3E",
    auto: "data:image/svg+xml,%3Csvg viewBox=%220 0 24 24%22 fill=%22none%22 xmlns=%22http://www.w3.org/2000/svg%22%3E%3Cpath d=%22M16.0365 9.38051C16.3253 9.70994 16.2924 10.2111 15.963 10.4999L12.5548 13.488C12.2253 13.7768 11.7241 13.7439 11.4353 13.4145C11.1465 13.085 11.1794 12.5838 11.5089 12.295L14.9171 9.30697C15.2465 9.01816 15.7477 9.05108 16.0365 9.38051Z%22 fill=%22%23000%22%3E%3C/path%3E%3Cpath fill-rule=%22evenodd%22 clip-rule=%22evenodd%22 d=%22M13.5817 1.19995C14.0198 1.19995 14.3749 1.55511 14.3749 1.99322C14.3749 2.43133 14.0198 2.78649 13.5817 2.78649L12.0481 2.78649V4.17598C17.0594 4.45038 21.0384 8.60089 21.0384 13.6807C21.0384 18.9381 16.7765 23.2 11.5192 23.2C6.26189 23.2 2 18.9381 2 13.6807C2 8.78094 5.70197 4.74577 10.4615 4.21963V2.78649L8.82205 2.78649C8.38394 2.78649 8.02878 2.43133 8.02878 1.99322C8.02878 1.55511 8.38394 1.19995 8.82205 1.19995H13.5817ZM11.5192 21.6134C15.9003 21.6134 19.4519 18.0618 19.4519 13.6807C19.4519 9.29965 15.9003 5.74807 11.5192 5.74807C7.13811 5.74807 3.58653 9.29965 3.58653 13.6807C3.58653 18.0618 7.13811 21.6134 11.5192 21.6134Z%22 fill=%22%23000%22%3E%3C/path%3E%3Cpath d=%22M22.0916 7.92024C22.3278 8.43778 21.9127 8.98699 21.3442 8.96714C21.0168 8.9557 20.7307 8.74964 20.5896 8.45397C19.8881 6.98431 18.7464 5.75998 17.3293 4.95771C17.0442 4.79631 16.8586 4.49659 16.87 4.16916C16.8899 3.60065 17.4667 3.22492 17.9665 3.49657C19.7815 4.48305 21.2342 6.04083 22.0916 7.92024Z%22 fill=%22%23000%22%3E%3C/path%3E%3C/svg%3E",
    data: "data:image/svg+xml,%3Csvg viewBox=%220 0 24 24%22 fill=%22none%22 xmlns=%22http://www.w3.org/2000/svg%22%3E%3Cpath fill-rule=%22evenodd%22 clip-rule=%22evenodd%22 d=%22M2 0.5C1.44772 0.5 1 0.947715 1 1.5V22.5C1 23.0523 1.44772 23.5 2 23.5H22C22.5523 23.5 23 23.0523 23 22.5V1.5C23 0.947715 22.5523 0.5 22 0.5H2ZM2.5 10.5V15.7H8V10.5H2.5ZM2.5 9H21.5V2H2.5V9ZM21.5 10.5H16.5V15.7H21.5V10.5ZM15 10.5H9.5V15.7H15V10.5ZM9.5 17.2H15V22H9.5V17.2ZM21.5 22H16.5V17.2H21.5V22ZM8 17.2H2.5V22H8V17.2Z%22 fill=%22%23000%22%3E%3C/path%3E%3C/svg%3E",
    adv: "data:image/svg+xml,%3Csvg viewBox=%220 0 24 24%22 fill=%22none%22 xmlns=%22http://www.w3.org/2000/svg%22%3E%3Cg%3E%3Cpath d=%22M13.1132 24H10.9685C10.5089 24 10.1209 23.6531 10.0698 23.1939L9.70213 21.0204C8.91574 20.847 8.12936 20.5306 7.39404 20.1021L5.5966 21.4082C5.46383 21.5 5.31064 21.551 5.14723 21.551C5.08596 21.551 4.93277 21.5714 4.74894 21.5204C4.6366 21.4898 4.52426 21.4286 4.43234 21.347L2.84936 19.847C2.84936 19.847 2.78809 19.7857 2.76766 19.7551C2.52255 19.4286 2.52255 18.9796 2.76766 18.6531L4.06468 16.8368C3.58468 16.1327 3.24766 15.3266 3.02298 14.4797L0.755745 14.0919C0.347234 14.0511 0 13.6633 0 13.2041V10.9695C0 10.5103 0.347234 10.1225 0.796596 10.0715L2.95149 9.70417C3.10468 8.90825 3.42128 8.09193 3.85021 7.33684L2.58383 5.53073C2.35915 5.2246 2.35915 4.77563 2.59404 4.4491C2.61447 4.41849 2.63489 4.38788 2.66553 4.36747L4.24851 2.76544C4.32 2.69401 4.41191 2.63278 4.50383 2.59197C4.7183 2.50013 4.91234 2.53074 5.00426 2.53074C5.17787 2.53074 5.32085 2.59197 5.45362 2.69401L7.22043 4.10216C7.89447 3.60217 8.70128 3.25523 9.53872 3.03074L9.94723 0.755244C9.98808 0.561367 10.1004 0.347083 10.2332 0.224635C10.4272 0.0511664 10.6723 -0.0304657 10.9277 0.0103504H13.0519C13.2357 0.0103504 13.5115 0.102187 13.6545 0.234839C13.8179 0.387899 13.92 0.581775 13.9506 0.79606L14.3183 2.96952C15.1353 3.13278 15.9728 3.46951 16.7489 3.91849L18.526 2.65319C18.6587 2.56136 18.8119 2.51034 18.9753 2.51034C19.0264 2.50013 19.2306 2.47972 19.4553 2.57156C19.5472 2.61238 19.6289 2.6634 19.7004 2.74503L21.2426 4.31645C21.2426 4.31645 21.2936 4.36747 21.314 4.39808C21.406 4.53073 21.4877 4.77563 21.4877 4.93889C21.4877 5.13277 21.4264 5.32664 21.314 5.4797L20.017 7.31643C20.497 8.0001 20.834 8.81642 21.0587 9.66335L23.234 10.0001C23.234 10.0001 23.2545 10.0001 23.2647 10.0001C23.4383 10.0409 23.6834 10.1838 23.7957 10.3266C23.9285 10.4899 24 10.694 24 10.898V13.1225C24 13.245 23.9694 13.3572 23.9183 13.4695C23.7549 13.796 23.3974 13.9694 23.0502 13.9592L20.9668 14.3164C20.7932 15.1327 20.446 15.9388 19.9966 16.6939L21.263 18.4796C21.3651 18.6327 21.4468 18.847 21.4468 19.0102C21.4468 19.2653 21.3345 19.5102 21.1506 19.6735L19.6187 21.2143C19.5268 21.2959 19.4247 21.3674 19.3021 21.4082C19.1183 21.4694 18.914 21.4694 18.7302 21.4082C18.6587 21.3878 18.5872 21.347 18.5157 21.3061L16.6877 20C15.9932 20.4898 15.1762 20.8265 14.3285 21.051L13.9915 23.2245C13.9506 23.6327 13.5626 23.9796 13.103 23.9796L13.1132 24ZM12.5004 22.9592C12.5004 22.9592 12.4902 23.0102 12.4902 23.0408V22.9592H12.5004ZM11.5915 22.9286V22.9898C11.5915 22.9898 11.5915 22.949 11.5915 22.9286ZM11.5098 22.4694H12.5821L12.8681 20.7653C12.9089 20.4898 13.0417 20.2245 13.2357 20.0102C13.4298 19.7959 13.6851 19.6531 13.9609 19.5817C14.6655 19.398 15.3396 19.1123 15.9728 18.7347C16.4221 18.449 17.0757 18.4388 17.5557 18.7347L18.9549 19.7347L19.7311 18.9592L18.72 17.5307C18.5464 17.2551 18.4647 16.9796 18.4647 16.7041C18.4647 16.4286 18.5464 16.1531 18.6894 15.9082C19.057 15.2858 19.343 14.6123 19.5268 13.898C19.5881 13.6327 19.7413 13.3776 19.9557 13.1735C20.1702 12.9797 20.4357 12.847 20.7319 12.8062L22.4783 12.5103V11.4286L20.783 11.1429C20.5072 11.1021 20.2417 10.9695 20.0272 10.7756C19.8128 10.5817 19.6698 10.3266 19.5983 10.0409C19.4145 9.34703 19.1285 8.66336 18.7506 8.03071C18.6179 7.81642 18.5362 7.54092 18.5362 7.2552C18.5362 6.96949 18.6179 6.69398 18.7711 6.45929L19.8026 5.01032L19.0162 4.2042L17.617 5.2042C17.0655 5.55113 16.463 5.53072 15.983 5.2246C15.36 4.86746 14.6553 4.58175 13.9302 4.40828C13.6545 4.34706 13.3991 4.194 13.2051 3.97971C13.0111 3.76543 12.8783 3.50012 12.8374 3.20421L12.5515 1.53075H11.3974L11.1013 3.25523C11.0809 3.51033 10.9481 3.80625 10.7438 4.04094C10.5294 4.27563 10.2536 4.43889 9.94723 4.51032C9.27319 4.694 8.59915 4.97971 7.96596 5.35726C7.53702 5.63277 6.85277 5.65317 6.37277 5.33685L4.98383 4.23481L4.17702 5.05114L5.18809 6.4797C5.3617 6.75521 5.4434 7.03072 5.4434 7.30622C5.4434 7.58173 5.3617 7.85724 5.21872 8.10214C4.8817 8.70417 4.61617 9.38784 4.44255 10.0919C4.38128 10.3776 4.22809 10.6327 4.01362 10.8266C3.79915 11.0307 3.53362 11.1531 3.23745 11.194L1.57277 11.4797V12.6531L3.28851 12.9388C3.55404 12.9695 3.85021 13.0919 4.08511 13.3062C4.32 13.5103 4.4834 13.7858 4.55489 14.1021C4.73872 14.7858 5.02468 15.4694 5.39234 16.1021C5.53532 16.3062 5.61702 16.5919 5.61702 16.8776C5.61702 17.1633 5.53532 17.449 5.37192 17.6939L4.36085 19.1123L5.15745 19.8674L6.58723 18.847C6.83234 18.6633 7.15915 18.5613 7.46553 18.5715C7.73106 18.5715 7.9966 18.6429 8.23149 18.7959C8.81362 19.1327 9.47745 19.4082 10.1719 19.5817C10.4374 19.6429 10.6928 19.7959 10.897 20.0102C11.0911 20.2245 11.2238 20.4898 11.2647 20.7755L11.5506 22.4592L11.5098 22.4694ZM4.03404 19.5306L4.01362 19.551C4.01362 19.551 4.02383 19.5408 4.03404 19.5306ZM0.970213 12.5715H1.05191C1.05191 12.5715 1.00085 12.5715 0.970213 12.5715ZM1.07234 11.5715H1.01106C1.01106 11.5715 1.05191 11.5715 1.07234 11.5715ZM3.84 4.63277L3.86043 4.65318C3.86043 4.65318 3.85021 4.63277 3.84 4.63277ZM13.0009 1.55116C13.0519 1.55116 13.103 1.55116 13.1643 1.55116H13.0009ZM11.9898 16.8368C11.04 16.8368 10.1209 16.5613 9.32425 16.0307C8.52766 15.5001 7.91489 14.7552 7.54723 13.8674C7.17957 12.9797 7.08766 12.0205 7.27149 11.0817C7.45532 10.1429 7.91489 9.2858 8.58894 8.61234C9.26298 7.93887 10.1209 7.47969 11.0604 7.29602C12 7.11235 12.96 7.20418 13.8485 7.57153C14.7268 7.93887 15.4723 8.55111 16.0034 9.35723C16.5345 10.1531 16.8102 11.0817 16.8102 12.0409C16.8102 13.3164 16.3098 14.5205 15.4009 15.4286C14.5021 16.3368 13.297 16.8368 12.0204 16.8368H12H11.9898ZM12 15.3062C12.8681 15.3062 13.6851 14.9592 14.3081 14.347C14.9311 13.7348 15.2681 12.9082 15.2681 12.0409C15.2681 11.3878 15.0843 10.7552 14.7166 10.2144C14.3591 9.67356 13.8485 9.25519 13.246 9.00009C12.6434 8.74499 11.9898 8.68377 11.3464 8.80622C10.703 8.92866 10.1311 9.24499 9.66128 9.70417C9.2017 10.1633 8.88511 10.745 8.76255 11.3878C8.64 12.0307 8.70128 12.6837 8.9566 13.2858C9.21192 13.8878 9.63064 14.398 10.1719 14.7552C10.7132 15.1123 11.3362 15.3062 11.9796 15.3062H11.9898H12Z%22 fill=%22%23000%22%3E%3C/path%3E%3C/g%3E%3C/svg%3E",
    about: "data:image/svg+xml,%3Csvg viewBox=%220 0 24 24%22 fill=%22none%22 xmlns=%22http://www.w3.org/2000/svg%22%3E%3Cpath fill-rule=%22evenodd%22 clip-rule=%22evenodd%22 d=%22M12.0012 13.993C13.1057 13.993 14.0012 13.0975 14.0012 11.993C14.0012 10.8884 13.1057 9.99295 12.0012 9.99295C10.8966 9.99295 10.0012 10.8884 10.0012 11.993C10.0012 13.0975 10.8966 13.993 12.0012 13.993ZM12.0012 15.493C13.9342 15.493 15.5012 13.9259 15.5012 11.993C15.5012 10.06 13.9342 8.49295 12.0012 8.49295C10.0682 8.49295 8.50116 10.06 8.50116 11.993C8.50116 13.9259 10.0682 15.493 12.0012 15.493Z%22 fill=%22%23000%22%3E%3C/path%3E%3Cpath fill-rule=%22evenodd%22 clip-rule=%22evenodd%22 d=%22M12.0012 2.58105C12.4154 2.58105 12.7512 2.91684 12.7512 3.33105V6.32623C12.7512 6.74044 12.4154 7.07623 12.0012 7.07623C11.5869 7.07623 11.2512 6.74044 11.2512 6.32623V3.33105C11.2512 2.91684 11.5869 2.58105 12.0012 2.58105Z%22 fill=%22%23000%22%3E%3C/path%3E%3Cpath fill-rule=%22evenodd%22 clip-rule=%22evenodd%22 d=%22M12.0012 16.9144C12.4154 16.9144 12.7512 17.2502 12.7512 17.6644V20.6691C12.7512 21.0833 12.4154 21.4191 12.0012 21.4191C11.5869 21.4191 11.2512 21.0833 11.2512 20.6691V17.6644C11.2512 17.2502 11.5869 16.9144 12.0012 16.9144Z%22 fill=%22%23000%22%3E%3C/path%3E%3Cpath fill-rule=%22evenodd%22 clip-rule=%22evenodd%22 d=%22M2.57007 11.9929C2.57007 11.5787 2.90585 11.2429 3.32007 11.2429H6.32976C6.74397 11.2429 7.07976 11.5787 7.07976 11.9929C7.07976 12.4071 6.74397 12.7429 6.32976 12.7429H3.32007C2.90585 12.7429 2.57007 12.4071 2.57007 11.9929Z%22 fill=%22%23000%22%3E%3C/path%3E%3Cpath fill-rule=%22evenodd%22 clip-rule=%22evenodd%22 d=%22M16.9226 11.9929C16.9226 11.5787 17.2584 11.2429 17.6726 11.2429H20.6798C21.0941 11.2429 21.4298 11.5787 21.4298 11.9929C21.4298 12.4071 21.0941 12.7429 20.6798 12.7429H17.6726C17.2584 12.7429 16.9226 12.4071 16.9226 11.9929Z%22 fill=%22%23000%22%3E%3C/path%3E%3Cpath fill-rule=%22evenodd%22 clip-rule=%22evenodd%22 d=%22M5.33185 18.6623C5.03896 18.3694 5.03896 17.8946 5.33185 17.6017L7.4606 15.4729C7.75349 15.18 8.22836 15.18 8.52126 15.4729C8.81415 15.7658 8.81415 16.2407 8.52126 16.5336L6.39251 18.6623C6.09962 18.9552 5.62475 18.9552 5.33185 18.6623Z%22 fill=%22%23000%22%3E%3C/path%3E%3Cpath fill-rule=%22evenodd%22 clip-rule=%22evenodd%22 d=%22M15.4812 8.51296C15.1883 8.22007 15.1883 7.74519 15.4812 7.4523L17.6019 5.33161C17.8948 5.03872 18.3697 5.03872 18.6626 5.33161C18.9555 5.6245 18.9555 6.09938 18.6626 6.39227L16.5419 8.51296C16.249 8.80585 15.7741 8.80585 15.4812 8.51296Z%22 fill=%22%23000%22%3E%3C/path%3E%3Cpath fill-rule=%22evenodd%22 clip-rule=%22evenodd%22 d=%22M18.6566 18.6485C18.3638 18.9414 17.8889 18.9414 17.596 18.6485L15.4811 16.5336C15.1882 16.2407 15.1882 15.7658 15.4811 15.4729C15.774 15.1801 16.2488 15.1801 16.5417 15.4729L18.6566 17.5879C18.9495 17.8808 18.9495 18.3556 18.6566 18.6485Z%22 fill=%22%23000%22%3E%3C/path%3E%3Cpath fill-rule=%22evenodd%22 clip-rule=%22evenodd%22 d=%22M8.5211 8.51298C8.2282 8.80587 7.75333 8.80587 7.46044 8.51298L5.33448 6.38702C5.04158 6.09413 5.04158 5.61925 5.33448 5.32636C5.62737 5.03347 6.10224 5.03347 6.39514 5.32636L8.5211 7.45232C8.81399 7.74521 8.81399 8.22009 8.5211 8.51298Z%22 fill=%22%23000%22%3E%3C/path%3E%3C/svg%3E",
  };

  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function badge(kind, text) {
    return el("span", `status-badge ${kind}`, text);
  }

  function textInput(value, placeholder) {
    const input = el("input", "settings-input");
    input.type = "text";
    input.value = value || "";
    input.placeholder = placeholder || "";
    input.spellcheck = false;
    return input;
  }

  function textArea(value, placeholder, rows) {
    const input = el("textarea", "settings-input");
    input.value = value || "";
    input.placeholder = placeholder || "";
    input.rows = rows || 3;
    input.spellcheck = false;
    return input;
  }

  // "" → null, which the service reads as "restore the default".
  function parseList(text) {
    const items = text.split(",").map((s) => s.trim()).filter(Boolean);
    return items.length > 0 ? items : null;
  }
  function parsePipelines(text) {
    const lines = text.split("\n").map((l) => l.trim()).filter(Boolean);
    if (lines.length === 0) return null;
    // A line that isn't exactly three names is sent as-is-ish so the service's
    // validation names the problem next to this field.
    return lines.map((line) => {
      const parts = line.split(/[\s,]+/).filter(Boolean);
      return {
        helmJob: parts[0] || "",
        appJob: parts[1] || "",
        browserJob: parts.length > 3 ? parts.slice(2).join(" ") : parts[2] || "",
      };
    });
  }
  // "job, KEY=VALUE, KEY=VALUE" per line -> [{ job, match }]. A malformed pair
  // keeps its text as a (bad) parameter name so the service's validation
  // names the problem next to this field.
  function parseAutJobs(text) {
    const lines = text.split("\n").map((l) => l.trim()).filter(Boolean);
    return lines.map((line) => {
      const [job, ...pairs] = line.split(",").map((p) => p.trim()).filter(Boolean);
      const match = {};
      for (const pair of pairs) {
        const eq = pair.indexOf("=");
        if (eq < 0) match[pair] = "";
        else match[pair.slice(0, eq).trim()] = pair.slice(eq + 1).trim();
      }
      return { job: job || "", match };
    });
  }
  function parseComponentMap(text) {
    const lines = text.split("\n").map((l) => l.trim()).filter(Boolean);
    if (lines.length === 0) return null;
    const map = {};
    for (const line of lines) {
      const at = line.indexOf("=");
      // A line with no "=" keeps its whole text as the name and an empty repo,
      // so the service's validation flags it next to this field.
      map[(at === -1 ? line : line.slice(0, at)).trim()] = at === -1 ? "" : line.slice(at + 1).trim();
    }
    return map;
  }

  // The three background watchers (companion-service/core/watchers.js).
  const WATCHER_LABELS = {
    conflicts: ["Conflicts on your pull requests", "Prepares a resolution in the background. Nothing is pushed until you approve it.", "You'll see: “Conflict on PR…”, when a PR of yours conflicts at a new commit (urgent if already approved)."],
    assignedBugs: ["Bugs assigned to you", "Runs a read-only analysis of each newly assigned bug, once.", "You'll see: “Analysis on the way for KEY…” (or “Assigned to you: KEY…” if it couldn't be started), once per ticket."],
    reviewRequests: ["Review requests", "Fetches the PR's branch into your clone. It never checks anything out.", "You'll see: “Review requested: PR…”, when you're a reviewer who hasn't voted, again after each new push."],
  };

  /** "last ran 5 min ago", "needs login", … for a watcher's status line. */
  function watcherStatusText(last) {
    if (!last) return "Hasn't run yet.";
    const mins = Math.max(0, Math.round((Date.now() - last.at) / 60000));
    const when = mins < 1 ? "just now" : `${mins} min ago`;
    if (last.needsLogin) return `Needs login — open the site in Chrome (or save a token above). Last tried ${when}.`;
    if (last.outcome === "failed") return `Failed ${when}: ${last.error || "no details"}`;
    return `Last ran ${when}.`;
  }

  function numberInput(value, min, max) {
    const input = el("input", "settings-input");
    input.type = "number";
    input.min = String(min);
    input.max = String(max);
    input.value = String(value);
    return input;
  }

  /** A native time picker: "HH:MM" in, "HH:MM" out, "" when empty. */
  function timeInput(value) {
    const input = el("input", "settings-input");
    input.type = "time";
    input.classList.add("settings-time");
    input.step = "60";
    input.value = value || "";
    return input;
  }

  // The pages of the left-hand navigation, in order. `sub` is the small line
  // under each title; the Features one is rewritten live with the count.
  const PAGES = [
    { id: "features", title: "Features", sub: "", intro: "Choose what the ✨ menu offers. Open a feature to change its own options. Turning a feature on or off takes effect once the companion service restarts." },
    { id: "conn", title: "Code and accounts", sub: "Repos, Claude, Jira, Jenkins", intro: "Connections shared by several features. Jira, Jenkins and Bitbucket use your browser login unless you add a token." },
    { id: "auto", title: "Background work", sub: "Watchers, limits, quiet hours", intro: "Off unless you turn it on. Results appear under ✨ → Ready for you, as Chrome notifications and in `companion inbox`." },
    { id: "data", title: "Privacy and history", sub: "Stored on this machine only", intro: "What the assistant keeps on this machine. Nothing leaves your computer." },
    { id: "adv", title: "Advanced", sub: "MCP server, connection", intro: "Rarely changed." },
    { id: "about", title: "About", sub: "Version, support", intro: "" },
  ];

  function renderSettingsForm(view, opts = {}) {
    const { settings, features, editors, defaults } = view;
    const branding = view.branding || {};
    const usedBy = usedByGroups();
    // The scheduler's status; the "background" probe adds the LLM backend's (see applyBackground).
    const background = { ...(view.background || {}) };
    const root = el("div", "settings-form");
    const fieldErrors = new Map();
    const groups = [];
    const pages = [];
    const pageById = new Map();
    let currentPage = "features";
    let built = false;

    // ---- Slow data, fetched when its page is first shown ----
    // Opening Settings only loads what was saved. What needs processes run or folders walked (is git
    // or claude usable, which editors are installed, the state of each mapped repository) is asked
    // of the service by page: each probe starts the first time a page that shows it is opened
    // (or a search could show it), and a failed one is tried again on the next visit.
    const probed = new Set();
    const loaders = {};
    const probeApplies = {};
    function probesFor(id) {
      if (id === "data") return [["dataFolder", (data) => probeApplies.dataFolder && probeApplies.dataFolder(data)]];
      if (id === "features") return [["checks", applyChecks], ["editors", applyEditors]];
      if (id === "conn") return [["repos", applyRepos]];
      if (extraPages.some((x) => x.def.id === id)) return [["background", applyBackground]];
      return [];
    }
    function loadProbes(id) {
      for (const [name, apply] of probesFor(id)) {
        if (probed.has(name)) continue;
        probed.add(name);
        Promise.resolve(loaders[name] ? loaders[name]() : typeof opts.loadProbe === "function" ? opts.loadProbe(name) : null)
          .then((data) => {
            if (data) apply(data, null);
            else probed.delete(name);
          })
          .catch((err) => {
            probed.delete(name);
            apply(null, err);
          });
      }
    }

    // ---- Frame: search, left navigation, the page area ----
    const searchRow = el("div", "settings-search-row");
    const searchInput = el("input", "settings-search");
    searchInput.type = "search";
    searchInput.placeholder = "Search all settings";
    searchInput.spellcheck = false;
    searchRow.appendChild(searchInput);
    const shell = el("div", "settings-shell");
    const nav = el("nav", "settings-nav");
    const main = el("div", "settings-main");
    const results = el("div", "settings-results");
    results.hidden = true;
    main.appendChild(results);
    shell.append(nav, main);
    root.append(searchRow, shell);

    // A page a pack registered (PaiSettingsPages) names and describes itself, usually from the distribution's branding.
    const extraDefs = new Map((self.PaiSettingsPages || []).map((def) => [def.id, def]));
    function pageMeta(id) {
      const def = extraDefs.get(id);
      if (def) return { id, title: def.title(branding), sub: def.sub || "", intro: def.intro ? def.intro(branding) : "" };
      return PAGES.find((p) => p.id === id);
    }

    function page(id) {
      const meta = pageMeta(id);
      const node = el("div", "settings-page");
      node.dataset.id = id;
      const head = el("div", "settings-page-head");
      head.append(el("div", "settings-page-title", meta.title), el("div", "settings-page-intro", meta.intro));
      node.appendChild(head);
      main.appendChild(node);
      const navItem = el("div", "settings-nav-item");
      navItem.tabIndex = 0;
      navItem.setAttribute("role", "button");
      const text = el("div", "settings-nav-text");
      const subNode = el("div", "settings-nav-sub", meta.sub);
      text.append(el("div", "settings-nav-title", meta.title), subNode);
      const dot = el("span", "settings-nav-dot");
      dot.hidden = true;
      const tile = el("span", "settings-nav-tile");
      const icon = (extraDefs.get(id) && extraDefs.get(id).icon) || PAGE_ICONS[id];
      if (icon) tile.style.setProperty("--pai-nav-icon", `url("${icon}")`);
      navItem.append(tile, text, dot);
      const go = () => showPage(id);
      navItem.addEventListener("click", go);
      navItem.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          go();
        }
      });
      nav.appendChild(navItem);
      const pg = { id, title: meta.title, node, navItem, subNode, dot, blocks: [], empty: null };
      pages.push(pg);
      pageById.set(id, pg);
      return pg;
    }

    function showPage(id) {
      if (searchInput.value) {
        searchInput.value = "";
        applySearch();
      }
      currentPage = id;
      loadProbes(id);
      for (const pg of pages) {
        pg.node.classList.toggle("on", pg.id === id);
        pg.navItem.classList.toggle("on", pg.id === id);
      }
      main.scrollTop = 0;
    }

    function clearErrors() {
      for (const box of root.querySelectorAll(".field-error")) {
        box.textContent = "";
        box.style.display = "none";
      }
      for (const input of root.querySelectorAll(".invalid")) input.classList.remove("invalid");
    }

    function errorBox() {
      const box = el("div", "field-error");
      box.style.display = "none";
      return box;
    }

    /** Brings the page, feature options, "Advanced" group or hidden repo row that holds `node` into view. */
    function reveal(node) {
      if (!node || !node.closest) return;
      if (!built) return;
      const pg = node.closest(".settings-page");
      if (pg && (searchInput.value || !pg.classList.contains("on"))) showPage(pg.dataset.id);
      const feature = node.closest(".settings-feature");
      if (feature) {
        feature.classList.add("open");
        if (feature.syncToggle) feature.syncToggle();
      }
      const details = node.closest("details");
      if (details) details.open = true;
      const repos = node.closest(".settings-repos");
      if (repos) repos.classList.remove("collapsed");
    }

    function showError(box, message, input) {
      box.textContent = box.textContent ? `${box.textContent}\n${message}` : message;
      box.style.display = "block";
      if (input) input.classList.add("invalid");
      // An error inside a collapsed group must not stay hidden.
      reveal(input || box);
    }

    /** A bordered section of a page: a title, optional tag, note and "Used by" line, then its fields. */
    function block(pg, title, { usedBy, note, status } = {}) {
      const node = el("section", "settings-section");
      node.appendChild(el("div", "settings-path", `${pg.title} › ${title}`));
      const head = el("div", "settings-block-head");
      const titleRow = el("div", "settings-block-title-row");
      titleRow.appendChild(el("div", "settings-section-title", title));
      let statusBadge = status ? badge(status.kind, status.text) : null;
      if (statusBadge) titleRow.appendChild(statusBadge);
      node.actionSlot = el("span", "settings-block-action");
      titleRow.appendChild(node.actionSlot);
      /** Replaces the section's status badge, for one that is only known after the page opened. */
      node.setStatus = (next) => {
        const replacement = badge(next.kind, next.text);
        if (statusBadge) statusBadge.replaceWith(replacement);
        else titleRow.insertBefore(replacement, node.actionSlot);
        statusBadge = replacement;
      };
      head.appendChild(titleRow);
      if (note) head.appendChild(el("div", "settings-note", note));
      if (usedBy) {
        const labels = features.filter((f) => usedBy.includes(f.id)).map((f) => f.label);
        if (labels.length > 0) head.appendChild(el("div", "settings-used-by", `Used by ${labels.join(", ")}`));
        groups.push({ node, usedBy });
      }
      node.appendChild(head);
      pg.node.appendChild(node);
      pg.blocks.push({ node });
      return node;
    }

    /** "Check every [ 15 ] minutes": a label, a narrow input and its unit on ONE line. */
    function inlineRow(parent, before, input, after, fieldName, hint) {
      const wrap = el("div", "settings-item");
      wrap.dataset.search = `${before} ${after || ""} ${hint || ""}`.toLowerCase();
      const row = el("div", "settings-row");
      row.appendChild(el("span", "settings-row-text", before));
      input.classList.add("settings-narrow");
      row.appendChild(input);
      if (after) row.appendChild(el("span", "settings-row-text", after));
      wrap.appendChild(row);
      if (hint) wrap.appendChild(el("p", "settings-hint settings-row-hint", hint));
      const box = errorBox();
      wrap.appendChild(box);
      parent.appendChild(wrap);
      if (fieldName) fieldErrors.set(fieldName, { box, input });
      return { wrap, row, box };
    }

    function field(parent, labelText, input, fieldName, hint) {
      const wrap = el("label", "settings-field settings-item");
      wrap.dataset.search = `${labelText} ${hint || ""}`.toLowerCase();
      wrap.appendChild(el("span", "settings-label", labelText));
      wrap.appendChild(input);
      if (hint) wrap.appendChild(el("span", "settings-hint", hint));
      const box = errorBox();
      wrap.appendChild(box);
      fieldErrors.set(fieldName, { box, input });
      parent.appendChild(wrap);
      return wrap;
    }

    /** A switch with its label and help beside it. `box` is the checkbox the caller reads. */
    function toggleRow(parent, labelText, box, hint, fieldName) {
      box.type = "checkbox";
      box.classList.add("settings-switch");
      const wrap = el("div", "settings-item");
      wrap.dataset.search = `${labelText} ${hint || ""}`.toLowerCase();
      const label = el("label", "settings-toggle");
      const text = el("span", "settings-toggle-text");
      text.appendChild(el("span", "settings-label", labelText));
      if (hint) text.appendChild(el("span", "settings-hint", hint));
      label.append(box, text);
      wrap.appendChild(label);
      const err = errorBox();
      wrap.appendChild(err);
      if (fieldName) fieldErrors.set(fieldName, { box: err, input: box });
      parent.appendChild(wrap);
      return wrap;
    }

    function tokenField(parent, labelText, fieldName, isSet, required = false) {
      const input = el("input", "settings-input");
      input.type = "password";
      input.autocomplete = "new-password";
      input.spellcheck = false;
      input.placeholder = isSet ? "Saved — leave blank to keep it" : required ? "Not set" : "Not set — your browser login is used";
      field(parent, labelText, input, `${fieldName}.apiToken`);
      const clear = el("input");
      if (isSet) {
        toggleRow(parent, "Remove the saved token", clear, null);
        clear.addEventListener("change", () => {
          input.disabled = clear.checked;
          if (clear.checked) input.value = "";
        });
      }
      return { input, clear };
    }

    const featurePage = page("features");
    const connPage = page("conn");
    const autoPage = settings.watchers ? page("auto") : null;
    const dataPage = page("data");
    // Pages a pack's extension script registered, between the data page and Advanced.
    const extraPages = [];
    for (const def of self.PaiSettingsPages || []) {
      try {
        if (def.visible(view)) extraPages.push({ def, page: page(def.id) });
      } catch (err) {
        console.warn(`[settings] the "${def.id}" page failed to show:`, err);
      }
    }
    const advPage = page("adv");
    const aboutPage = page("about");

    // ---- Features ----
    // One row per feature: a switch, its name and status, and an "Options"
    // link that opens the settings only that feature uses.
    const featureList = el("div", "settings-features");
    featurePage.node.appendChild(featureList);
    featurePage.blocks.push({ node: featureList, custom: true });
    const featureRows = new Map();
    // The features with checks to pass, until the "checks" probe says which do.
    const featureChecks = [];
    function applyChecks(data, err) {
      const checks = (data && data.checks) || {};
      for (const entry of featureChecks) {
        for (const node of entry.problems) node.remove();
        entry.problems = [];
        if (err) {
          entry.status.className = "status-badge neutral";
          entry.status.textContent = "Not checked";
          entry.status.title = err.message;
          continue;
        }
        const failing = entry.feature.requiredChecks.filter((name) => checks[name] && !checks[name].ok);
        entry.status.title = "";
        entry.status.className = `status-badge ${failing.length > 0 ? "warn" : "ok"}`;
        entry.status.textContent = failing.length > 0 ? `Needs ${failing.map((n) => CHECK_LABELS[n] || n).join(", ")}` : "Ready";
        for (const name of failing) {
          const problem = el("div", "settings-feature-problem", checks[name].message);
          entry.text.appendChild(problem);
          entry.problems.push(problem);
        }
      }
    }
    for (const feature of features) {
      const row = el("div", "settings-feature");
      const head = el("div", "settings-feature-head");
      const label = el("label", "settings-feature-label");
      const box = el("input", "settings-switch");
      box.type = "checkbox";
      box.checked = settings.enabledFeatures.includes(feature.id);
      const text = el("div", "settings-feature-text");
      const title = el("div", "settings-feature-title", feature.label);
      const status = badge("neutral", feature.requiredChecks.length > 0 ? "Checking…" : "Ready");
      title.appendChild(document.createTextNode(" "));
      title.appendChild(status);
      text.append(title, el("div", "settings-feature-desc", feature.description));
      if (feature.requiredChecks.length > 0) featureChecks.push({ feature, status, text, problems: [] });
      const entry = PaiRegistry.all().find((e) => e.id === feature.id);
      const tile = el("span", "settings-feature-tile");
      tile.style.setProperty("--pai-row-icon", PaiIcons.mask((entry && entry.icon) || "AI"));
      label.append(box, tile, text);
      const toggle = el("button", "settings-link-button", "Options");
      toggle.type = "button";
      toggle.hidden = true;
      row.syncToggle = () => {
        toggle.textContent = row.classList.contains("open") ? "Hide options" : toggle.dataset.closed || "Options";
      };
      toggle.addEventListener("click", () => {
        row.classList.toggle("open");
        row.syncToggle();
      });
      head.append(label, toggle);
      const optionsEl = el("div", "settings-feature-options");
      const path = el("div", "settings-feature-path", `Features › ${feature.label}`);
      row.append(head, path, optionsEl);
      featureList.appendChild(row);
      featureRows.set(feature.id, {
        feature,
        row,
        head,
        box,
        toggle,
        optionsEl,
        own: `${feature.label} ${feature.description}`.toLowerCase(),
      });
    }
    const featuresError = errorBox();
    featurePage.node.appendChild(featuresError);
    fieldErrors.set("enabledFeatures", { box: featuresError });

    /** Where a feature's own settings go. A feature this service doesn't list gets a detached box, so its values still round-trip. */
    function optionsOf(id) {
      const entry = featureRows.get(id);
      return entry ? entry.optionsEl : el("div");
    }

    // Review PR.
    const editorSelect = el("select", "settings-input");
    // Only the saved choice is listed until the "editors" probe says which are installed.
    function fillEditors(detectedIds) {
      const ids = detectedIds || [];
      const chosen = editorSelect.value;
      editorSelect.textContent = "";
      if (!settings.reviewEditor) editorSelect.appendChild(new Option("Pick automatically (first one installed)", ""));
      for (const editor of editors.supported) {
        const installed = ids.includes(editor.id);
        if (!installed && editor.id !== settings.reviewEditor) continue;
        const label = installed || !detectedIds ? editor.label : `${editor.label} — not found on this computer`;
        editorSelect.appendChild(new Option(label, editor.id, false, editor.id === settings.reviewEditor));
      }
      editorSelect.value = chosen || settings.reviewEditor || "";
    }
    fillEditors(null);
    const editorField = field(optionsOf("review-in-editor"), 'Open "Review PR" checkouts in', editorSelect, "reviewEditor");
    function applyEditors(data) {
      if (!data) return;
      const detectedIds = data.detected.map((e) => e.id);
      fillEditors(detectedIds);
      if (detectedIds.length === 0) editorField.insertBefore(el("span", "settings-hint", "No supported editor (Cursor or Claude Code) was found on this computer."), editorField.lastChild);
    }

    // Diagnose failed builds: the optional risk-facts/v1 feed (core/risk-facts.js).
    // `settings.riskFacts` is absent from an older companion service.
    let riskUrlInput = null;
    if (settings.riskFacts) {
      riskUrlInput = textInput(settings.riskFacts.url, "https://…/risk-facts.json");
      field(
        optionsOf("diagnose-build"),
        "Known-flaky test feed URL",
        riskUrlInput,
        "riskFacts.url",
        "Optional. A risk-facts/v1 JSON feed of known-flaky tests; “Diagnose this failed build” shows what it says. Leave blank to turn it off.",
      );
    }

    // Pre-deployment build stats. What a blank field falls back to comes from the pack that owns it.
    const defaultPipeline = ((defaults.jenkinsExtras || {}).pipelines || [])[0];
    const defaultPipelineText = defaultPipeline
      ? [defaultPipeline.helmJob, defaultPipeline.appJob, defaultPipeline.browserJob].join(", ")
      : "helm job, sample-app job, browser job";
    const pipelinesInput = textArea(
      (settings.jenkins.pipelines || []).map((p) => [p.helmJob, p.appJob, p.browserJob].join(", ")).join("\n"),
      defaultPipelineText,
      3,
    );
    field(
      optionsOf("pre-deployment-stats"),
      "Pipelines",
      pipelinesInput,
      "jenkins.pipelines",
      `One per line: helm job, sample-app job, browser job. Leave blank for the default (${defaultPipelineText}).`,
    );

    const defaultAut = ((defaults.jenkinsExtras || {}).autJobs || [])[0];
    const defaultAutText = defaultAut ? [defaultAut.job, ...Object.entries(defaultAut.match || {}).map(([k, v]) => `${k}=${v}`)].join(", ") : "job, KEY=VALUE";
    const autJobsInput = textArea(
      (settings.jenkins.autJobs || [])
        .map((a) => [a.job, ...Object.entries(a.match || {}).map(([k, v]) => `${k}=${v}`)].join(", "))
        .join("\n"),
      defaultAutText,
      3,
    );
    field(
      optionsOf("pre-deployment-stats"),
      "Automation jobs",
      autJobsInput,
      "jenkins.autJobs",
      "One per line: job, then KEY=VALUE build parameters a run must carry to count, e.g. my_aut_job, BRANCH=master. Shown as a warning beside each build, never as a verdict. Clear all lines for none; this list is saved as-is (blank is not “default”).",
    );

    // Summarize comments (the model field only; absent from an older service's view).
    const summarizeModelInput = textInput(settings.summarizeComments?.model || "", "claude-haiku-4-5-20251001 (Haiku)");
    field(optionsOf("summarize-comments"), "Claude model for this feature", summarizeModelInput, "summarizeComments.model", "Blank = Haiku, which is plenty for a summary.");

    // Address review comments (the model field only; absent from an older service's view).
    const addressModelInput = textInput(settings.addressReviewComments?.model || "", "Default model");
    field(
      optionsOf("address-review-comments"),
      "Model for this feature",
      addressModelInput,
      "addressReviewComments.model",
      "Blank = the tool's own default. Runs in Cursor Agent when Cursor is your Review PR editor (use a Cursor model name, e.g. gpt-5), otherwise in Claude Code (a Claude model id).",
    );

    // Analyze a Jira issue.
    const analyzeOptions = optionsOf("analyze-issue");
    const analyzeView = settings.analyzeIssue;
    const analyzeModelInput = textInput(analyzeView.model, "Same as the Claude model on Code and accounts");
    field(analyzeOptions, "Claude model for this feature", analyzeModelInput, "analyzeIssue.model");
    // The target fields are absent from an older companion service's view.
    const defaultProjects = defaults.analyzeProjects || [];
    const projectsInput = textInput((analyzeView.projects || []).join(", "), defaultProjects.slice(0, 2).join(", ") || "PROJ");
    field(
      analyzeOptions,
      "Jira projects",
      projectsInput,
      "analyzeIssue.projects",
      `Comma-separated project keys. Blank = ${defaultProjects.length ? defaultProjects.join(", ") : "every project"}.`,
    );
    const typesInput = textInput((analyzeView.issueTypes || []).join(", "), "Any type");
    field(analyzeOptions, "Issue types", typesInput, "analyzeIssue.issueTypes", "Comma-separated to limit to some types (e.g. Bug, Incident). Blank = any type.");
    const componentsInput = textArea(
      Object.entries(analyzeView.componentRepoMap || {})
        .map(([component, repo]) => `${component} = ${repo}`)
        .join("\n"),
      "Component = PROJECT/repo",
      3,
    );
    field(
      analyzeOptions,
      "Components → repos (learned automatically)",
      componentsInput,
      "analyzeIssue.componentRepoMap",
      "Filled in as tickets are analyzed. Edit or delete a line to correct it; add one (Jira component = PROJECT/repo) to set it yourself.",
    );
    // Similar past tickets (Phase 8): `settings.similar` is absent from an
    // older companion service, so the switch (and its key) is skipped then.
    let similarCheck = null;
    if (settings.similar) {
      similarCheck = el("input");
      similarCheck.checked = settings.similar.enabled !== false;
      toggleRow(
        analyzeOptions,
        "Add similar past tickets",
        similarCheck,
        "From this machine's history, marked as earlier AI output. When an LLM is set up (see its page), matches by meaning; otherwise by shared words.",
        "similar.enabled",
      );
    }

    // Ticket to PR. `settings.ticketToPr` is absent from an older companion service.
    let reviewTransitionInput = null;
    let autoMoveCheck = null;
    let createWorktreeCheck = null;
    if (settings.ticketToPr) {
      const ticketOptions = optionsOf("ticket-to-pr");
      reviewTransitionInput = textInput(settings.ticketToPr.reviewTransitionName, "In Review");
      field(
        ticketOptions,
        "Review transition",
        reviewTransitionInput,
        "ticketToPr.reviewTransitionName",
        "The Jira transition (or status) the ticket is moved with once its pull request is open. Skipped when the ticket has none by this name.",
      );
      autoMoveCheck = el("input");
      autoMoveCheck.checked = settings.ticketToPr.autoMoveToReview !== false;
      toggleRow(
        ticketOptions,
        "Move the ticket to review automatically",
        autoMoveCheck,
        "Checks every few minutes for a pull request from a fix you started, and moves the ticket when there is one. Off: use the Move ticket to review button.",
      );
      createWorktreeCheck = el("input");
      createWorktreeCheck.checked = settings.ticketToPr.createWorktreeWithoutAsking !== false;
      toggleRow(
        ticketOptions,
        "Create a worktree without asking",
        createWorktreeCheck,
        "Continue in Claude Code reuses a checkout of the ticket's branch when there is one. On (default): once the repository is identified, it makes a new worktree and branch straight away. Off: it asks first.",
      );
    }

    // Morning digest.
    let digestBox = null;
    let digestTime = null;
    if (settings.digest) {
      const digestOptions = optionsOf("digest");
      digestBox = el("input");
      digestBox.checked = !!settings.digest.enabled;
      toggleRow(digestOptions, "Also post a digest on weekdays", digestBox, null, "digest.enabled");
      digestTime = timeInput(settings.digest.time || "08:30");
      inlineRow(digestOptions, "Post at", digestTime, "(your local time)", "digest.time");
      const syncDigest = () => {
        digestTime.disabled = !digestBox.checked;
      };
      digestBox.addEventListener("change", syncDigest);
      syncDigest();
    }

    for (const { toggle, optionsEl } of featureRows.values()) {
      const n = optionsEl.querySelectorAll("[data-search]").length;
      if (n === 0) continue;
      toggle.hidden = false;
      toggle.dataset.closed = n === 1 ? "Options" : `${n} options`;
      toggle.textContent = toggle.dataset.closed;
    }

    // ---- Code and accounts ----
    const repoSection = block(connPage, "Repositories", {
      usedBy: features.filter((f) => f.needsRepos).map((f) => f.id),
      note:
        'Local clones, as "PROJECT/repo" and folder. You don\'t have to list every repo — the first time you use ' +
        "one that's missing, you're offered to pick its folder or have it cloned.",
    });
    const repoCount = badge("ok", "");
    repoSection.actionSlot.append(repoCount);
    const repoRows = [];
    const repoList = el("div", "settings-repos");
    repoSection.appendChild(repoList);
    const REPOS_SHOWN = 5;
    const repoMore = el("button", "settings-link-button settings-repos-more");
    repoMore.type = "button";

    function syncRepoList() {
      repoCount.textContent = `${repoRows.length} ${repoRows.length === 1 ? "repository" : "repositories"}`;
      const collapsible = repoRows.length > REPOS_SHOWN;
      repoMore.hidden = !collapsible;
      if (!collapsible) repoList.classList.remove("collapsed");
      repoMore.textContent = repoList.classList.contains("collapsed") ? `Show all ${repoRows.length}` : "Show fewer";
    }
    repoMore.addEventListener("click", () => {
      repoList.classList.toggle("collapsed");
      syncRepoList();
    });

    // `status` is the probe's answer; `pending` marks a saved row still waiting for it.
    function addRepoRow(key, clonePath, status, pending = false) {
      const row = el("div", "settings-repo settings-item");
      row.dataset.search = `repositories ${key} ${clonePath}`.toLowerCase();
      const line = el("div", "settings-repo-line");
      const keyInput = textInput(key, "PROJECT/repo");
      keyInput.className += " settings-repo-key";
      const pathInput = textInput(clonePath, "/Users/you/gitviews/repo");
      pathInput.className += " settings-repo-path";
      const state = status
        ? badge(status.ok ? "ok" : "bad", status.ok ? "OK" : "Problem")
        : badge("neutral", pending ? "Checking…" : "Checked on save");
      if (status && !status.ok) state.title = status.message;
      const remove = el("button", "subtask-row-remove", "×");
      remove.type = "button";
      remove.title = "Remove";
      const box = errorBox();
      if (status && !status.ok) showError(box, status.message);
      const entry = { row, keyInput, pathInput, box, state, edited: false };
      remove.addEventListener("click", () => {
        if (baseline.get(keyInput) || baseline.get(pathInput)) removedRepos += 1;
        row.remove();
        repoRows.splice(repoRows.indexOf(entry), 1);
        syncRepoList();
        recompute();
      });
      for (const input of [keyInput, pathInput]) {
        input.addEventListener("input", () => {
          entry.edited = true;
          state.className = "status-badge neutral";
          state.textContent = "Checked on save";
          state.title = "";
        });
      }
      line.append(keyInput, pathInput, state, remove);
      row.append(line, box);
      repoList.appendChild(row);
      repoRows.push(entry);
      syncRepoList();
      return entry;
    }

    for (const [key, clonePath] of Object.entries(settings.repos)) addRepoRow(key, clonePath, null, true);
    if (repoRows.length > REPOS_SHOWN) repoList.classList.add("collapsed");
    syncRepoList();
    repoSection.appendChild(repoMore);
    const addRepo = el("button", "settings-btn", "Add repository");
    addRepo.type = "button";
    addRepo.addEventListener("click", () => {
      repoList.classList.remove("collapsed");
      addRepoRow("", "", null).keyInput.focus();
      recompute();
    });
    repoSection.actionSlot.append(addRepo);

    // The "repos" probe: each saved row's state, and the clones found on this computer.
    function applyRepos(data, err) {
      if (!data) {
        for (const entry of repoRows) {
          if (entry.edited || entry.state.textContent !== "Checking…") continue;
          entry.state.className = "status-badge neutral";
          entry.state.textContent = "Not checked";
          entry.state.title = err ? err.message : "";
        }
        return;
      }
      const { repoStatus, suggestedRepos } = data;
      for (const entry of repoRows) {
        const status = repoStatus[entry.keyInput.value];
        if (entry.edited || !status) continue;
        entry.state.className = `status-badge ${status.ok ? "ok" : "bad"}`;
        entry.state.textContent = status.ok ? "OK" : "Problem";
        entry.state.title = status.ok ? "" : status.message;
        // Not showError(): the person may have moved to another page since the question was asked.
        if (!status.ok) {
          entry.box.textContent = status.message;
          entry.box.style.display = "block";
        }
      }
      const unlisted = suggestedRepos.filter((clone) => !repoRows.some((r) => r.keyInput.value === clone.key));
      if (unlisted.length === 0) return;
      const found = el("details", "settings-found");
      found.appendChild(el("summary", null, `${unlisted.length} more clone(s) found on this computer`));
      for (const clone of unlisted) {
        const item = el("div", "settings-found-item");
        const text = el("div", "settings-found-text");
        text.append(el("span", "settings-found-key", clone.key), el("span", "settings-found-path", clone.path));
        const add = el("button", "settings-btn", "Add");
        add.type = "button";
        add.addEventListener("click", () => {
          repoList.classList.remove("collapsed");
          addRepoRow(clone.key, clone.path, null);
          item.remove();
          recompute();
        });
        item.append(text, add);
        found.appendChild(item);
      }
      repoSection.appendChild(found);
    }

    const jiraSection = block(connPage, "Jira", {
      usedBy: usedBy.jira,
      status: settings.jira.apiTokenSet ? { kind: "ok", text: "API token saved" } : settings.jira.apiTokenExternal ? { kind: "ok", text: `Using token from ${settings.jira.apiTokenExternal}` } : { kind: "ok", text: "Using browser login" },
      note: "An API token is only needed as a fallback for when your browser login doesn't work.",
    });
    const jiraUrl = textInput(settings.jira.baseUrl, defaults.jiraBaseUrl);
    field(jiraSection, "Jira base URL", jiraUrl, "jira.baseUrl");
    const jiraToken = tokenField(jiraSection, "Jira API token", "jira", settings.jira.apiTokenSet);

    const jenkinsSection = block(connPage, "Jenkins", {
      usedBy: usedBy.jenkins,
      status: settings.jenkins.apiTokenSet ? { kind: "ok", text: "API token saved" } : settings.jenkins.apiTokenExternal ? { kind: "ok", text: `Using token from ${settings.jenkins.apiTokenExternal}` } : { kind: "ok", text: "Using browser login" },
      note: "A username and API token are only needed as a fallback for when your browser login doesn't work.",
    });
    const jenkinsUrl = textInput(settings.jenkins.baseUrl, defaults.jenkinsBaseUrl);
    field(jenkinsSection, "Jenkins base URL", jenkinsUrl, "jenkins.baseUrl");
    const jenkinsUser = textInput(settings.jenkins.username, "Leave blank to use your browser login");
    field(jenkinsSection, "Jenkins username", jenkinsUser, "jenkins.username");
    const jenkinsToken = tokenField(jenkinsSection, "Jenkins API token", "jenkins", settings.jenkins.apiTokenSet);

    // The git host is whichever site of kind "git" the profile names (Bitbucket, GitHub, ...): its own name, its own
    // settings key. GitHub's browser login is no use to the companion, so it works with a token only.
    const gitSite = (view.sites || []).find((s) => s.kind === "git") || { id: "bitbucket", label: "Bitbucket", tokenOnly: false };
    const gitView = settings[gitSite.id] || { baseUrl: "", apiTokenSet: false, apiTokenExternal: "" };
    const gitDefaultUrl = (defaults.siteBaseUrls && defaults.siteBaseUrls[gitSite.id]) || defaults.bitbucketBaseUrl || "";
    const gitSection = block(connPage, gitSite.label, {
      usedBy: usedBy.git,
      status: gitView.apiTokenSet
        ? { kind: "ok", text: "API token saved" }
        : gitView.apiTokenExternal
          ? { kind: "ok", text: `Using token from ${gitView.apiTokenExternal}` }
          : gitSite.tokenOnly
            ? { kind: "warn", text: "A personal access token is required" }
            : { kind: "ok", text: "Using browser login" },
      note: gitSite.tokenOnly
        ? `${gitSite.label} can only be used with a personal access token (the browser login doesn't work with its API). Create one with access to the repositories you work on — read and write for pull requests and contents — and paste it below.`
        : "An API token is only needed as a fallback for when your browser login doesn't work.",
    });
    const gitUrl = textInput(gitView.baseUrl, gitDefaultUrl);
    field(gitSection, `${gitSite.label} base URL`, gitUrl, `${gitSite.id}.baseUrl`, gitSite.tokenOnly ? "Blank for github.com; for GitHub Enterprise, the address you open in the browser (not the API address)." : undefined);
    const gitToken = tokenField(gitSection, `${gitSite.label} ${gitSite.tokenOnly ? "personal access token" : "API token"}`, gitSite.id, gitView.apiTokenSet, gitSite.tokenOnly);

    connPage.empty = el("div", "settings-empty", "Nothing to set up here until you turn on a feature that uses a repository, Claude, Jira, Jenkins or your git host.");
    connPage.node.appendChild(connPage.empty);

    // ---- Background work ----
    // Opt-in watchers, the daily Claude budget, quiet hours, notification
    // batching and the pre-push check. All read live by the service — no
    // restart. `settings.watchers` is absent from an older companion
    // service, so the whole page is skipped then.
    let backgroundUpdate = null;
    if (autoPage) {
      const scheduler = background.scheduler || {};

      const watchers = block(autoPage, "Watchers", {
        note: "Each watcher checks on a schedule and prepares work in the background. Nothing is pushed until you approve it.",
      });
      const watcherInputs = {};
      for (const [name, [label, hint, expect]] of Object.entries(WATCHER_LABELS)) {
        const saved = settings.watchers[name] || { enabled: false, intervalMinutes: 30 };
        const box = el("input");
        box.checked = !!saved.enabled;
        toggleRow(watchers, label, box, `${hint} ${expect}`, `watchers.${name}.enabled`);
        const interval = numberInput(saved.intervalMinutes, 1, 1440);
        const last = scheduler.watchers && scheduler.watchers[name] ? scheduler.watchers[name].last : null;
        const { wrap } = inlineRow(watchers, "Check every", interval, "minutes", `watchers.${name}.intervalMinutes`, saved.enabled ? watcherStatusText(last) : null);
        wrap.classList.add("settings-indent");
        wrap.dataset.search += ` ${label}`.toLowerCase();
        // The timer only matters while the watcher is on.
        const syncInterval = () => wrap.classList.toggle("is-off", !box.checked);
        box.addEventListener("change", syncInterval);
        syncInterval();
        watcherInputs[name] = { box, interval };
      }

      const limits = block(autoPage, "Limits and quiet time");
      const budgetInput = numberInput(settings.budget.claudeRunsPerDay, 0, 50);
      const used = scheduler.budget ? ` Used today: ${scheduler.budget.used} of ${scheduler.budget.limit}.` : "";
      inlineRow(limits, "Background Claude runs:", budgetInput, "per day", "budget.claudeRunsPerDay", `0 turns background Claude runs off.${used}`);

      const quiet = settings.scheduler.quietHours || { start: "", end: "" };
      const quietStart = timeInput(quiet.start);
      const quietEnd = timeInput(quiet.end);
      const quietRow = inlineRow(
        limits,
        "Quiet hours: from",
        quietStart,
        "to",
        "scheduler.quietHours",
        "Optional. No background runs and no notifications in this window; what's held goes into the digest. Leave both empty for none.",
      );
      quietEnd.classList.add("settings-narrow");
      quietRow.row.appendChild(quietEnd);
      const clearQuiet = el("button", "settings-link-button", "Clear");
      clearQuiet.type = "button";
      clearQuiet.addEventListener("click", () => {
        quietStart.value = "";
        quietEnd.value = "";
        recompute();
      });
      quietRow.row.appendChild(clearQuiet);
      fieldErrors.set("scheduler.quietHours.end", { box: quietRow.box, input: quietEnd });

      const notifyInput = numberInput(settings.notify.minIntervalMinutes, 0, 1440);
      inlineRow(limits, "Notify at most once every", notifyInput, "minutes", "notify.minIntervalMinutes", "Urgent ones (a conflict on an approved PR) come at once.");

      const pushCard = block(autoPage, "Pre-push check", {
        note:
          "When you run git push, warns you if the files in it have a history of test regressions, using the shared test-history feed " +
          "(set under Features › Diagnose failed builds). It only warns, never blocks the push, and stays silent for low risk or when the companion isn't running.",
      });
      const prePushSelect = el("select", "settings-input");
      for (const [label, value] of [
        ["Warn (never blocks the push)", "warn"],
        ["Off", "off"],
      ]) {
        prePushSelect.appendChild(new Option(label, value, false, settings.prePush.mode === value));
      }
      field(pushCard, "Mode", prePushSelect, "prePush.mode", "Warn shows the warning on risky pushes. Off turns it off in every repository.");
      const steps = el("div", "settings-item");
      steps.dataset.search = "pre-push check set up install hook companion hooks install COMPANION_SKIP skip once repository terminal";
      steps.appendChild(el("div", "settings-label", "Set up and skip"));
      const list = el("ol", "settings-hint settings-list");
      for (const parts of [
        ["Per repository, once: open a terminal in that repository's root folder and run ", ["code", "companion hooks install"], ". It adds the hook to that repo's own git config and keeps any existing hook, such as husky."],
        ["Check or remove it with ", ["code", "companion hooks status"], " or ", ["code", "companion hooks uninstall"], " in the same folder, or pass a path: ", ["code", "companion hooks install /path/to/repo"], "."],
        ["Skip it for a single push by putting the variable in front of the command, in the terminal where you push: ", ["code", "COMPANION_SKIP=1 git push"], ". It is not a setting to save here."],
      ]) {
        const li = el("li");
        for (const part of parts) {
          if (typeof part === "string") li.appendChild(document.createTextNode(part));
          else li.appendChild(el(part[0], null, part[1]));
        }
        list.appendChild(li);
      }
      steps.appendChild(list);
      pushCard.appendChild(steps);
      backgroundUpdate = () => ({
        watchers: Object.fromEntries(
          Object.entries(watcherInputs).map(([name, { box, interval }]) => [name, { enabled: box.checked, intervalMinutes: Number(interval.value) }]),
        ),
        budget: { claudeRunsPerDay: Number(budgetInput.value) },
        scheduler: {
          quietHours: quietStart.value.trim() || quietEnd.value.trim() ? { start: quietStart.value.trim(), end: quietEnd.value.trim() } : null,
        },
        notify: { minIntervalMinutes: Number(notifyInput.value) },
        prePush: { mode: prePushSelect.value },
      });
    }

    // ---- Privacy and history ----
    // Feeds the companion's in-memory session vault (core/session-vault.js)
    // so terminal Claude, talking to the companion as an MCP server, can
    // reuse this browser's Jira/Jenkins/Bitbucket logins. Not gated to a
    // feature — it's used by the MCP server, not the ✨ menu.
    const sessionSection = block(dataPage, "Browser sessions");
    const sessionTtlInput = el("input", "settings-input");
    sessionTtlInput.type = "number";
    sessionTtlInput.min = "0";
    sessionTtlInput.max = "1440";
    // `settings.sessionCache` is absent from an older companion service —
    // fall back to the section defaults rather than throwing.
    const sessionCache = settings.sessionCache || { ttlMinutes: 30, heartbeat: false };
    sessionTtlInput.value = String(sessionCache.ttlMinutes);
    sessionTtlInput.classList.add("settings-narrow");
    field(
      sessionSection,
      "Keep sessions for (minutes)",
      sessionTtlInput,
      "sessionCache.ttlMinutes",
      "0 turns this off. Sessions are kept in memory only, never on disk.",
    );
    const heartbeatCheck = el("input");
    heartbeatCheck.checked = !!sessionCache.heartbeat;
    toggleRow(
      sessionSection,
      "Keep browser sessions warm (checks every 5 minutes)",
      heartbeatCheck,
      "Lets Claude Code in your terminal use your Jira, Jenkins and Bitbucket logins while Chrome is open. Off by default.",
      "sessionCache.heartbeat",
    );

    // How long the companion's SQLite history (history.db) keeps rows.
    // `settings.history` is absent from an older companion service, so the
    // section (and its key in getUpdate) is skipped then.
    let historyDaysInput = null;
    if (settings.history) {
      const historySection = block(dataPage, "Local history");
      historyDaysInput = el("input", "settings-input settings-narrow");
      historyDaysInput.type = "number";
      historyDaysInput.min = "7";
      historyDaysInput.max = "3650";
      historyDaysInput.value = String(settings.history.retentionDays);
      field(
        historySection,
        "Keep history for (days)",
        historyDaysInput,
        "history.retentionDays",
        "Older runs are removed from history.db on this machine. Nothing leaves your computer.",
      );
    }

    // ---- Data folder ----
    // Everything the assistant stores on this machine (job worktrees, analysis
    // clones, history). The size comes from the service; Purge clears only
    // the rebuildable checkouts (core/data-folder.js). Skipped against an
    // older service that has no /data-folder route.
    const dataFolderSection = block(dataPage, "Data folder", {
      note: "Where worktrees, analysis checkouts and history are kept. Purging removes the checkouts only; history, settings and logins stay.",
    });
    const formatBytes = (n) => {
      if (n < 1024) return `${n} B`;
      const units = ["KB", "MB", "GB", "TB"];
      let v = n / 1024;
      let i = 0;
      while (v >= 1024 && i < units.length - 1) {
        v /= 1024;
        i++;
      }
      return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
    };
    const dataRow = (label, valueNode) => {
      const wrap = el("div", "settings-item");
      wrap.dataset.search = `${label} data folder worktrees purge size disk`.toLowerCase();
      const row = el("div", "settings-row");
      row.append(el("span", "settings-row-text", label), valueNode);
      wrap.appendChild(row);
      dataFolderSection.appendChild(wrap);
    };
    const dataPath = el("span", "settings-row-text", "Checking…");
    const dataSize = el("span", "settings-row-text", "Checking…");
    const dataPurgeable = el("span", "settings-row-text", "");
    dataRow("Location", dataPath);
    dataRow("Total size", dataSize);
    dataRow("Can be purged", dataPurgeable);
    const dataActions = el("div", "settings-row");
    const openFolder = el("button", "settings-link-button", "Open folder");
    const purgeData = el("button", "settings-link-button", "Purge data");
    purgeData.disabled = true;
    const dataMessage = el("div", "settings-note", "");
    dataActions.append(openFolder, purgeData);
    dataFolderSection.append(dataActions, dataMessage);
    const showUsage = (usage) => {
      dataPath.textContent = usage.path;
      dataSize.textContent = formatBytes(usage.totalBytes);
      dataPurgeable.textContent = formatBytes(usage.purgeableBytes);
      purgeData.disabled = usage.purgeableBytes === 0;
    };
    const dataFailure = (response) => (chrome.runtime.lastError ? chrome.runtime.lastError.message : response && response.error);
    // Walks every file under the data folder, so it is asked for only when the Data page is opened.
    const applyDataFolder = (response) => {
      const failure = dataFailure(response);
      if (failure || !response || !response.usage) {
        dataPath.textContent = "Not available";
        dataSize.textContent = "Not available";
        dataMessage.textContent = failure ? String(failure) : "";
        return;
      }
      showUsage(response.usage);
    };
    loaders.dataFolder = () =>
      new Promise((resolve) => chrome.runtime.sendMessage({ type: "data-folder-get" }, (response) => resolve(response || {})));
    probeApplies.dataFolder = applyDataFolder;
    openFolder.addEventListener("click", () => {
      dataMessage.textContent = "";
      chrome.runtime.sendMessage({ type: "data-folder-open" }, (response) => {
        const failure = dataFailure(response);
        if (failure) dataMessage.textContent = `Couldn't open the folder: ${failure}`;
      });
    });
    purgeData.addEventListener("click", () => {
      if (
        !confirm(
          "Delete all worktrees and analysis checkouts?\n\nHistory, settings and logins are kept. Results still waiting for your approval and any resumable sessions in those checkouts will be lost. They can be re-run.",
        )
      ) {
        return;
      }
      purgeData.disabled = true;
      dataMessage.textContent = "Purging…";
      chrome.runtime.sendMessage({ type: "data-folder-purge" }, (response) => {
        const failure = dataFailure(response);
        if (failure) {
          dataMessage.textContent = `Couldn't purge: ${failure}`;
          purgeData.disabled = false;
          return;
        }
        dataMessage.textContent = `Freed ${formatBytes(response.freedBytes || 0)}.`;
        if (response.usage) showUsage(response.usage);
      });
    });

    // ---- Pages and settings sections a pack's extension script registers (PaiSettingsPages) ----
    // Each one builds its own page from the same helpers the built-in pages use and returns a function
    // giving its part of the update (see PaiSettingsPages at the end of this file).
    const extraUpdates = [];
    // A page that shows the LLM backend's status asks for it here and is called once the probe answers
    // (with the error, and the status unchanged, when it can't be had).
    const backgroundListeners = [];
    const whenBackground = (listener) => backgroundListeners.push(listener);
    function applyBackground(data, err) {
      if (data) Object.assign(background, data.background);
      for (const listener of backgroundListeners) {
        try {
          listener(background, err);
        } catch (e) {
          console.warn("[settings] a page's status failed to update:", e);
        }
      }
    }
    for (const { def, page: pg } of extraPages) {
      try {
        const collect = def.build({ page: pg, view, settings, branding, background, whenBackground, el, block, field, toggleRow, textInput });
        if (typeof collect === "function") extraUpdates.push(collect);
      } catch (err) {
        console.warn(`[settings] the "${def.id}" page failed to build:`, err);
      }
    }

    // ---- Advanced: MCP server, connection ----
    // The companion's own MCP server (companion-service/core/mcp.ts), which
    // terminal Claude Code / Cursor call with a token of its own — never
    // shown here, only whether one is saved. Not gated to a feature, same
    // as Browser sessions above. `settings.mcp` is absent from an older
    // companion service, so this section (and its key in getUpdate) is
    // skipped then rather than sending a setting it would reject.
    if (settings.mcp) {
      const mcpSection = block(advPage, "MCP server");
      const tokenItem = el("div", "settings-item");
      tokenItem.dataset.search = "mcp token rotate";
      const tokenStatus = el("p", "settings-hint", `Token: ${settings.mcp.tokenSet ? "set" : "not set"}`);
      tokenItem.appendChild(tokenStatus);
      const rotate = el("button", "settings-btn", "Rotate token");
      rotate.type = "button";
      const rotateResult = el("div", "settings-hint");
      const rotateError = errorBox();
      rotate.addEventListener("click", () => {
        if (
          !confirm(
            "Rotate the MCP token? Claude Code and Cursor are re-registered automatically; other clients need the new token.",
          )
        ) {
          return;
        }
        rotate.disabled = true;
        rotateResult.textContent = "";
        rotateError.textContent = "";
        rotateError.style.display = "none";
        // Saves only the rotation, not the rest of the form — nothing else
        // on this panel is touched by it.
        chrome.runtime.sendMessage({ type: "settings-save", settings: { mcp: { rotateToken: true } } }, (response) => {
          rotate.disabled = false;
          const failure = chrome.runtime.lastError ? chrome.runtime.lastError.message : response && response.error;
          if (failure) {
            showError(rotateError, failure);
            return;
          }
          const saved = (response && response.view) || {};
          tokenStatus.textContent = `Token: ${saved.settings?.mcp?.tokenSet ? "set" : "not set"}`;
          const lines = ["Token rotated."];
          const registration = saved.mcpRegistration;
          if (registration) {
            for (const [client, label] of [
              ["claude", "Claude Code"],
              ["cursor", "Cursor"],
            ]) {
              if (registration[client]) lines.push(`${label}: ${registration[client].message}`);
            }
          }
          rotateResult.textContent = lines.join("\n");
          rotateResult.style.whiteSpace = "pre-line";
        });
      });
      tokenItem.append(rotate, rotateResult, rotateError);
      mcpSection.appendChild(tokenItem);
    }

    // ---- Connection (read-only) ----
    const connection = block(advPage, "Connection", {
      note:
        "The extension reaches the companion service on this port using a shared secret, so neither can be " +
        "changed from here — that would disconnect this panel. To change them, re-run `npm run setup` in the " +
        "companion-service folder.",
    });
    const portInput = textInput(String(settings.port));
    portInput.readOnly = true;
    portInput.classList.add("settings-narrow");
    field(connection, "Port", portInput, "port");

    // ---- About ----
    const SUPPORT_EMAIL = branding.supportEmail || "";
    const about = block(aboutPage, chrome.runtime.getManifest().name);
    let version = "unknown";
    try {
      version = chrome.runtime.getManifest().version;
    } catch {
      // Not running as an extension page; leave the version as unknown.
    }
    const aboutItem = (label, value) => {
      const wrap = el("div", "settings-item");
      wrap.dataset.search = `${label} ${typeof value === "string" ? value : SUPPORT_EMAIL}`.toLowerCase();
      const row = el("div", "settings-row");
      row.append(el("span", "settings-row-text", label), typeof value === "string" ? el("span", "settings-row-text", value) : value);
      wrap.appendChild(row);
      about.appendChild(wrap);
    };
    aboutItem("Version", version);
    if (SUPPORT_EMAIL) {
      const mail = el("a", "settings-link", SUPPORT_EMAIL);
      mail.href = `mailto:${SUPPORT_EMAIL}?subject=${encodeURIComponent(`${chrome.runtime.getManifest().name} ${version} support`)}`;
      aboutItem("Support", mail);
    }

    // ---- Check for updates ----
    const updateBtn = el("button", "settings-btn", "Check for updates");
    updateBtn.type = "button";
    const updateStatus = el("span", "settings-row-text", "");
    const updateCell = el("span", "settings-row-text");
    updateCell.append(updateStatus, " ", updateBtn);
    aboutItem("Updates", updateCell);

    const bgMessage = (message) =>
      new Promise((resolve, reject) => {
        chrome.runtime.sendMessage(message, (response) => {
          const failure = chrome.runtime.lastError ? chrome.runtime.lastError.message : response && response.error;
          if (failure) reject(new Error(String(failure)));
          else resolve(response);
        });
      });
    // ---- Change log: this install's own CHANGELOG.md, always here (What's new in the update panel only
    // exists while an update is on offer). No refresh, so opening About never waits on git. The last copy
    // seen is kept in the browser and shown at once, then replaced if the service says it changed (it only
    // does when an update was installed), so this page never waits on the service at all but the first time. ----
    const changelog = block(aboutPage, "Change log");
    const changelogBody = el("div", "settings-item");
    changelogBody.appendChild(el("div", "settings-row-text", "Loading…"));
    changelog.appendChild(changelogBody);
    const CHANGELOG_CACHE_KEY = "paiChangelogCache";
    let shownChangelog = null;
    let changelogItems = [];
    const showChangelog = (info) => {
      const entries = (Array.isArray(info.changelog) ? info.changelog : []).filter(
        (e) => e && typeof e.version === "string" && Array.isArray(e.notes) && e.notes.length > 0,
      );
      const fingerprint = JSON.stringify([info.current, entries]);
      if (fingerprint === shownChangelog) return;
      shownChangelog = fingerprint;
      for (const item of changelogItems) item.remove();
      changelogItems = [];
      if (entries.length === 0) {
        changelogBody.textContent = "No change log in this install.";
        changelog.appendChild(changelogBody);
        return;
      }
      changelogBody.remove();
      for (const entry of entries) {
        const item = el("div", "settings-item");
        item.dataset.search = `change log ${entry.version} ${entry.notes.join(" ")}`.toLowerCase();
        const head = el("div", "settings-changelog-version", `v${entry.version}`);
        if (entry.version === info.current) head.appendChild(el("span", "settings-changelog-installed", " · installed"));
        const list = el("ul", "settings-changelog-notes");
        for (const note of entry.notes) list.appendChild(el("li", "", String(note)));
        item.append(head, list);
        changelog.appendChild(item);
        changelogItems.push(item);
      }
    };
    const cachedChangelog = new Promise((resolve) => {
      try {
        chrome.storage.local.get([CHANGELOG_CACHE_KEY], (r) => resolve((r && r[CHANGELOG_CACHE_KEY]) || null));
      } catch {
        resolve(null);
      }
    });
    (async () => {
      let fresh = false;
      cachedChangelog.then((cached) => {
        if (cached && !fresh) showChangelog(cached);
      });
      try {
        const { info } = await bgMessage({ type: "update-info" });
        fresh = true;
        showChangelog(info);
        try {
          chrome.storage.local.set({ [CHANGELOG_CACHE_KEY]: { current: info.current, changelog: info.changelog } });
        } catch {
          // Only a convenience: the next visit just asks the service again.
        }
      } catch {
        if (!(await cachedChangelog)) changelogBody.textContent = "Couldn't load the change log (is the companion service running?).";
      }
    })();

    let offeredLatest = null;
    const checkForUpdates = async () => {
      updateBtn.disabled = true;
      updateBtn.classList.remove("go");
      updateStatus.classList.remove("settings-ok");
      updateStatus.textContent = "Checking…";
      try {
        const { info } = await bgMessage({ type: "update-info", refresh: true });
        const run = info.update;
        if (run && run.state === "running") {
          updateStatus.textContent = "An update is already running…";
        } else if (info.available) {
          offeredLatest = info.latest;
          if (!info.canApply) {
            updateStatus.textContent = `v${info.latest} is available. ${info.cannotApplyReason} Update by hand: ${info.manualCommand}`;
          } else {
            updateStatus.textContent = `v${info.latest} is available.`;
            updateStatus.classList.add("settings-ok");
            updateBtn.classList.add("go");
            updateBtn.textContent = `Update to v${info.latest}`;
          }
        } else if (info.error) {
          // A failed check keeps the previous answer, so "up to date" would be a guess.
          offeredLatest = null;
          updateStatus.textContent = `Couldn't check for updates: ${info.error}`;
        } else {
          offeredLatest = null;
          updateBtn.textContent = "Check for updates";
          updateStatus.textContent = `You're up to date (v${info.current}).`;
        }
      } catch (err) {
        updateStatus.textContent = `Couldn't check: ${err.message}`;
      }
      updateBtn.disabled = false;
    };
    const applyUpdate = async () => {
      updateBtn.disabled = true;
      updateStatus.classList.remove("settings-ok");
      updateStatus.textContent = "Starting the update…";
      try {
        await bgMessage({ type: "update-apply" });
        const deadline = Date.now() + 10 * 60 * 1000;
        while (Date.now() < deadline) {
          await new Promise((resolve) => setTimeout(resolve, 2000));
          let res;
          try {
            res = await bgMessage({ type: "update-info" });
          } catch {
            updateStatus.textContent = "Restarting the companion service…";
            continue;
          }
          const run = res.info.update;
          if (!run || run.state === "running") {
            updateStatus.textContent = (run && run.label) || "Updating…";
          } else if (run.state === "failed") {
            updateStatus.textContent = run.error || "The update failed.";
            break;
          } else if (run.needsRestart) {
            updateStatus.textContent = `${run.label} Restart the companion service yourself (cd ~/ai-dev-companion/companion-service && npm start), then reload the extension.`;
            return;
          } else {
            updateStatus.textContent =
              `Updated to v${res.info.current}. Click "Reload extension and page" to finish. ` +
              "If it doesn't reload, open chrome://extensions and click the reload (↻) icon on " + chrome.runtime.getManifest().name + ". " +
              "Refresh any other open Bitbucket, Jira or Jenkins tabs too.";
            updateBtn.textContent = "Reload extension and page";
            updateBtn.classList.add("go");
            updateStatus.classList.add("settings-ok");
            updateBtn.disabled = false;
            offeredLatest = null;
            updateBtn.onclick = () => {
              updateBtn.disabled = true;
              updateStatus.textContent = "Reloading the extension…";
              bgMessage({ type: "reload-extension" }).catch(() => {});
              setTimeout(() => location.reload(), 1500);
            };
            return;
          }
        }
        if (updateStatus.textContent === "Updating…") updateStatus.textContent = "The update is taking longer than expected; see ~/.ai-dev-companion/update.log.";
      } catch (err) {
        updateStatus.textContent = `Update failed: ${err.message}`;
      }
      updateBtn.disabled = false;
    };
    updateBtn.onclick = () => (offeredLatest ? applyUpdate() : checkForUpdates());

    function selectedFeatureIds() {
      return features.filter((f) => featureRows.get(f.id).box.checked).map((f) => f.id);
    }

    /** Hides a group while none of the features that use it is ticked. */
    function updateGroups() {
      const enabled = selectedFeatureIds();
      for (const g of groups) g.node.classList.toggle("gated-off", !g.usedBy.some((id) => enabled.includes(id)));
      for (const pg of pages) {
        if (pg.empty) pg.empty.hidden = pg.blocks.some((b) => !b.node.classList.contains("gated-off"));
      }
    }

    // ---- Search ----
    // Every input lives in the DOM all the time; searching just shows the
    // ones whose label or help text matches, each under its page › section.
    function applySearch() {
      const q = searchInput.value.trim().toLowerCase();
      const searching = q !== "";
      if (searching) for (const id of ["features", "conn", ...extraPages.map((x) => x.def.id)]) loadProbes(id);
      root.classList.toggle("searching", searching);
      let total = 0;
      for (const pg of pages) {
        let pageCount = 0;
        for (const b of pg.blocks) {
          if (b.custom) {
            let n = 0;
            for (const { row, head, optionsEl, own } of featureRows.values()) {
              const ownMatch = searching && own.includes(q);
              let optionMatches = 0;
              for (const it of optionsEl.querySelectorAll("[data-search]")) {
                const m = !searching || it.dataset.search.includes(q);
                it.hidden = !m;
                if (searching && m) optionMatches += 1;
              }
              row.hidden = searching && !ownMatch && optionMatches === 0;
              head.hidden = searching && !ownMatch;
              row.classList.toggle("force-open", searching && optionMatches > 0);
              if (searching) n += (ownMatch ? 1 : 0) + optionMatches;
            }
            b.node.hidden = searching && n === 0;
            pageCount += n;
            continue;
          }
          const gatedOff = b.node.classList.contains("gated-off");
          let n = 0;
          for (const it of b.node.querySelectorAll("[data-search]")) {
            const m = !searching || (!gatedOff && it.dataset.search.includes(q));
            it.hidden = !m;
            if (searching && m) n += 1;
          }
          b.node.hidden = searching && n === 0;
          pageCount += n;
        }
        pg.node.hidden = searching && pageCount === 0;
        total += pageCount;
      }
      for (const pg of pages) pg.navItem.classList.toggle("on", !searching && pg.id === currentPage);
      results.hidden = !searching;
      results.textContent = searching
        ? total === 0
          ? `No settings match “${searchInput.value.trim()}”`
          : `${total} ${total === 1 ? "setting" : "settings"} matching “${searchInput.value.trim()}”`
        : "";
      if (!searching) main.scrollTop = 0;
    }
    searchInput.addEventListener("input", applySearch);
    searchInput.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && searchInput.value) {
        e.stopPropagation();
        searchInput.value = "";
        applySearch();
      }
    });

    // ---- Unsaved-change tracking ----
    // A control is dirty when it differs from what it held when the form was
    // built; a row added afterwards starts from "". Removing a saved repo
    // row counts as a change to its page.
    const baseline = new Map();
    let removedRepos = 0;
    const valueOf = (c) => (c.type === "checkbox" ? String(c.checked) : c.value);
    const controlsOf = (node) => node.querySelectorAll("input, select, textarea");
    let lastCount = -1;
    let scheduled = false;

    function recompute() {
      const featureCount = features.filter((f) => featureRows.get(f.id).box.checked).length;
      featurePage.subNode.textContent = `${featureCount} of ${features.length} on`;
      let count = 0;
      for (const pg of pages) {
        let n = pg.id === "conn" ? removedRepos : 0;
        for (const c of controlsOf(pg.node)) {
          const base = baseline.has(c) ? baseline.get(c) : c.type === "checkbox" ? "false" : "";
          if (valueOf(c) !== base) n += 1;
        }
        pg.dot.hidden = n === 0;
        count += n;
      }
      if (count !== lastCount) {
        lastCount = count;
        if (form.onDirtyChange) form.onDirtyChange(count);
      }
    }
    function scheduleRecompute() {
      if (scheduled) return;
      scheduled = true;
      setTimeout(() => {
        scheduled = false;
        recompute();
      }, 0);
    }
    main.addEventListener("input", scheduleRecompute);
    main.addEventListener("change", scheduleRecompute);
    main.addEventListener("click", scheduleRecompute);

    for (const box of [...featureRows.values()].map((r) => r.box)) box.addEventListener("change", updateGroups);
    updateGroups();

    function getUpdate() {
      clearErrors();
      const repos = {};
      let incomplete = false;
      for (const { keyInput, pathInput, box } of repoRows) {
        const key = keyInput.value.trim();
        const clonePath = pathInput.value.trim();
        if (!key && !clonePath) continue;
        if (!key || !clonePath) {
          showError(box, "Needs both a PROJECT/repo and a folder.", key ? pathInput : keyInput);
          incomplete = true;
          continue;
        }
        repos[key] = clonePath;
      }
      if (incomplete) throw new Error("Some repositories are missing a name or a folder.");
      return {
        enabledFeatures: selectedFeatureIds(),
        repos,
        reviewEditor: editorSelect.value || null,
        jira: { baseUrl: jiraUrl.value, apiToken: jiraToken.input.value, clearApiToken: jiraToken.clear.checked },
        jenkins: {
          baseUrl: jenkinsUrl.value,
          username: jenkinsUser.value,
          apiToken: jenkinsToken.input.value,
          clearApiToken: jenkinsToken.clear.checked,
          pipelines: parsePipelines(pipelinesInput.value),
          autJobs: parseAutJobs(autJobsInput.value),
        },
        [gitSite.id]: {
          baseUrl: gitUrl.value,
          apiToken: gitToken.input.value,
          clearApiToken: gitToken.clear.checked,
        },
        analyzeIssue: {
          model: analyzeModelInput.value,
          projects: parseList(projectsInput.value),
          issueTypes: parseList(typesInput.value),
          componentRepoMap: parseComponentMap(componentsInput.value),
        },
        ...(settings.summarizeComments ? { summarizeComments: { model: summarizeModelInput.value } } : {}),
        ...(settings.addressReviewComments ? { addressReviewComments: { model: addressModelInput.value } } : {}),
        sessionCache: { ttlMinutes: Number(sessionTtlInput.value), heartbeat: heartbeatCheck.checked },
        ...(historyDaysInput ? { history: { retentionDays: Number(historyDaysInput.value) } } : {}),
        ...(similarCheck ? { similar: { enabled: similarCheck.checked } } : {}),
        ...(riskUrlInput ? { riskFacts: { url: riskUrlInput.value.trim() } } : {}),
        ...(reviewTransitionInput ? { ticketToPr: { reviewTransitionName: reviewTransitionInput.value.trim(), ...(autoMoveCheck ? { autoMoveToReview: autoMoveCheck.checked } : {}), ...(createWorktreeCheck ? { createWorktreeWithoutAsking: createWorktreeCheck.checked } : {}) } } : {}),
        ...(digestBox ? { digest: { enabled: digestBox.checked, time: digestTime.value.trim() || "08:30" } } : {}),
        ...(backgroundUpdate ? backgroundUpdate() : {}),
        ...Object.assign({}, ...extraUpdates.map((collect) => collect())),
      };
    }

    /** Marks each server-side error on its field; returns the messages that
     * don't belong to any field shown here. */
    function showErrors(errors) {
      clearErrors();
      const unmatched = [];
      for (const { field: name, message } of errors) {
        const target = fieldErrors.get(name);
        if (target) {
          showError(target.box, message, target.input);
          continue;
        }
        const repo = name.startsWith("repos.") && repoRows.find((r) => r.keyInput.value.trim() === name.slice(6));
        if (repo) {
          showError(repo.box, message, repo.pathInput);
          continue;
        }
        unmatched.push(name ? `${name} ${message}` : message);
      }
      const first = root.querySelector(".field-error[style*='block']");
      if (first) first.scrollIntoView({ block: "center", behavior: "smooth" });
      return unmatched;
    }

    const form = { node: root, getUpdate, showErrors, onDirtyChange: null, currentPage: () => currentPage };
    for (const c of controlsOf(root)) if (c !== searchInput) baseline.set(c, valueOf(c));
    built = true;
    showPage(pageById.has(opts.page) ? opts.page : "features");
    recompute();
    return form;
  }

  window.renderSettingsForm = renderSettingsForm;

  // A pack's extension script adds a page to this panel by pushing a definition onto self.PaiSettingsPages
  // (the panel reads the list each time it opens, so load order doesn't matter):
  //   { id, icon?: dataUri, title(branding), sub?, intro?(branding), visible(view) -> bool,
  //     build({ page, view, settings, branding, background, whenBackground, el, block, field, toggleRow, textInput })
  //       -> () => the part of the PUT /settings body this page owns }
  // `background` is what is cheap to know when Settings opens (the scheduler). What is slow (the LLM backend's
  // status, a network call) arrives when the page is first shown: `whenBackground(listener)` calls
  // `listener(background, error)` then, and a section's badge can be replaced with `block.setStatus({ kind, text })`.
  // `settings.<key>` is what a SettingsSection of that pack returns from GET /settings; the server validates
  // and saves what the returned function gives.
  self.PaiSettingsPages = self.PaiSettingsPages || [];
})();
