<?php

// Real Magento discovers a module through ComponentRegistrar, not by
// finding etc/module.xml. Without this the module is invisible to
// `bin/magento`, so the fixture would have no verifiable ground truth.
\Magento\Framework\Component\ComponentRegistrar::register(
    \Magento\Framework\Component\ComponentRegistrar::MODULE,
    'Acme_Web',
    __DIR__
);
